//! The bounded JSON-RPC transport both Arkiv backends use: one global concurrency gate, a
//! per-request budget (calls and cumulative bytes), a per-response size bound and the optional
//! debug trace.
use crate::backend::BackendError;
use anyhow::{bail, ensure, Context, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::sync::Semaphore;

/// Limits of a transport, from the backend configuration.
#[derive(Clone, Debug)]
pub(crate) struct Limits {
    pub rpc_timeout: Duration,
    pub max_rpc_concurrency: usize,
    pub max_rpc_requests: usize,
    pub max_rpc_response_bytes: usize,
    pub max_read_bytes: usize,
}
impl Limits {
    pub(crate) fn check(&self) -> Result<()> {
        ensure!(
            self.max_rpc_concurrency > 0
                && self.max_rpc_concurrency <= Semaphore::MAX_PERMITS
                && self.max_rpc_requests > 0
                && self.max_rpc_response_bytes > 0
                && self.max_read_bytes > 0
                && !self.rpc_timeout.is_zero(),
            "RPC limits must be positive"
        );
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct Counters {
    pub requests: AtomicU64,
    pub bytes: AtomicU64,
}
/// Per-request work: calls, cumulative response bytes and the optional debug trace.
#[derive(Default)]
pub(crate) struct Budget {
    pub trace: Option<crate::trace::RpcTrace>,
    pub requests: AtomicU64,
    pub bytes: AtomicU64,
}
pub(crate) struct Transport {
    url: String,
    api_key: Option<String>,
    /// Attempts after a busy answer (HTTP 429 or RPC error -32010 `ServerBusy`), 100 ms apart
    /// and doubling; 0 makes busy an ordinary failure. Every attempt counts against the budget.
    busy_retries: u32,
    client: reqwest::Client,
    slots: Semaphore,
    next_id: AtomicU64,
    pub limits: Limits,
    pub counters: Counters,
}
/// Checks an RPC URL: HTTP(S), a host, no credentials, query or fragment.
pub(crate) fn check_url(rpc_url: &str) -> Result<()> {
    let url = reqwest::Url::parse(rpc_url).context("invalid RPC URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "RPC URL must be HTTP(S) without credentials, query or fragment"
    );
    Ok(())
}
impl Transport {
    pub(crate) fn new(
        url: String,
        api_key: Option<String>,
        busy_retries: u32,
        limits: Limits,
    ) -> Result<Self> {
        limits.check()?;
        check_url(&url)?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(limits.rpc_timeout)
            .user_agent(concat!("arkiv-sourcify-server/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            url,
            api_key,
            busy_retries,
            client,
            slots: Semaphore::new(limits.max_rpc_concurrency),
            next_id: AtomicU64::new(1),
            limits,
            counters: Counters::default(),
        })
    }
    /// One call; an RPC error is `Unavailable`.
    pub(crate) async fn rpc(&self, budget: &Budget, method: &str, params: Value) -> Result<Value> {
        self.call(budget, method, params)
            .await?
            .map_err(|_| BackendError::Unavailable(format!("{method} returned RPC error")).into())
    }
    /// One call; the outer error is transport, envelope or budget failure, the inner one the
    /// node's RPC error object. A busy node is asked again up to `busy_retries` times.
    pub(crate) async fn call(
        &self,
        budget: &Budget,
        method: &str,
        params: Value,
    ) -> Result<std::result::Result<Value, Value>> {
        let (mut delay, mut retries) = (Duration::from_millis(100), self.busy_retries);
        loop {
            let retry = retries > 0;
            match self.attempt(budget, method, params.clone()).await? {
                Attempt::Busy {
                    cost_limited: false,
                    ..
                } if retry => {}
                Attempt::Busy {
                    status,
                    cost_limited,
                } => {
                    bail!(BackendError::Unavailable(if cost_limited {
                        format!("{method}: HTTP {status}, the anonymous cost limit (set ARKIV_RPC_API_KEY)")
                    } else {
                        format!("{method}: HTTP {status}")
                    }))
                }
                Attempt::Done(Err(error)) if retry && error["code"] == BUSY_CODE => {}
                Attempt::Done(result) => return Ok(result),
            }
            tokio::time::sleep(delay).await;
            delay *= 2;
            retries -= 1;
        }
    }
    async fn attempt(&self, budget: &Budget, method: &str, params: Value) -> Result<Attempt> {
        ensure!(
            budget.requests.fetch_add(1, Ordering::Relaxed) < self.limits.max_rpc_requests as u64,
            BackendError::WorkLimit("RPC request limit exceeded".into())
        );
        let _permit = self
            .slots
            .acquire()
            .await
            .context("RPC concurrency gate closed")?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.counters.requests.fetch_add(1, Ordering::Relaxed);
        let call = budget
            .trace
            .as_ref()
            .map(|trace| trace.start("arkiv", method, &params));
        let result = async {
            let mut request = self
                .client
                .post(&self.url)
                .header("Accept-Encoding", "identity")
                .header("Accept", "application/json");
            if let Some(key) = &self.api_key {
                request = request.header("X-API-KEY", key);
            }
            let response = request
                .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .send()
                .await
                .map_err(|_| BackendError::Unavailable(format!("{method} transport failed")))?;
            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let status = response.status().to_string();
                // Our node's 429 means busy; Jalebi's anonymous cost limit (its window resets in
                // minutes) is not worth asking again. Only the first chunk is read.
                let mut response = response;
                let head = response.chunk().await.ok().flatten().unwrap_or_default();
                let cost_limited = head.windows(12).any(|w| w == b"COST_LIMITED");
                return Ok(Attempt::Busy {
                    status,
                    cost_limited,
                });
            }
            ensure!(
                response.status().is_success(),
                BackendError::Unavailable(format!("{method}: HTTP {}", response.status()))
            );
            ensure!(
                response
                    .content_length()
                    .is_none_or(|n| n <= self.limits.max_rpc_response_bytes as u64),
                BackendError::WorkLimit("RPC response exceeds configured size".into())
            );
            let mut body = Vec::new();
            let mut chunks = response.bytes_stream();
            while let Some(chunk) = chunks.next().await {
                let chunk = chunk
                    .map_err(|_| BackendError::Unavailable("RPC response body failed".into()))?;
                ensure!(
                    chunk.len()
                        <= self
                            .limits
                            .max_rpc_response_bytes
                            .saturating_sub(body.len()),
                    BackendError::WorkLimit("RPC response exceeds configured size".into())
                );
                let previous = budget
                    .bytes
                    .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                self.counters
                    .bytes
                    .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                ensure!(
                    previous.saturating_add(chunk.len() as u64)
                        <= self.limits.max_read_bytes as u64,
                    BackendError::WorkLimit("RPC cumulative byte limit exceeded".into())
                );
                body.extend_from_slice(&chunk);
            }
            let envelope: Value = serde_json::from_slice(&body).context("invalid RPC JSON")?;
            ensure!(
                envelope.is_object()
                    && envelope["jsonrpc"] == "2.0"
                    && envelope["id"] == id
                    && (envelope.get("result").is_some() != envelope.get("error").is_some()),
                BackendError::Corrupt("invalid JSON-RPC envelope or response ID".into())
            );
            Ok(Attempt::Done(match envelope.get("error") {
                Some(error) => Err(error.clone()),
                None => Ok(envelope["result"].clone()),
            }))
        }
        .await;
        if let Some(call) = call {
            call.finish(matches!(result, Ok(Attempt::Done(Ok(_)))));
        }
        result
    }
}
/// The RPC error code of a node that is too busy to answer now (our node's `ServerBusy`).
const BUSY_CODE: i64 = -32010;
enum Attempt {
    /// HTTP 429, with its status text and whether the body names a cost limit.
    Busy {
        status: String,
        cost_limited: bool,
    },
    Done(std::result::Result<Value, Value>),
}
