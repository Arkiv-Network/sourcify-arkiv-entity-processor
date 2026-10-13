//! Bounded Ethereum reads for proxy resolution. URLs are never included in errors.
use crate::{backend::ReadFuture, proxy::Provider};
use anyhow::{ensure, Context, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::Semaphore;

#[derive(Clone)]
pub struct Config {
    pub rpc_url: String,
    pub rpc_timeout: Duration,
    pub max_rpc_concurrency: usize,
    pub max_rpc_requests: usize,
    pub max_rpc_response_bytes: usize,
    pub max_read_bytes: usize,
    pub max_implementations: usize,
}
impl Config {
    pub fn new(rpc_url: String) -> Self {
        Self {
            rpc_url,
            rpc_timeout: Duration::from_secs(15),
            max_rpc_concurrency: 16,
            max_rpc_requests: 64,
            max_rpc_response_bytes: 1024 * 1024,
            max_read_bytes: 4 * 1024 * 1024,
            max_implementations: 128,
        }
    }
}
struct Inner {
    config: Config,
    client: reqwest::Client,
    slots: Semaphore,
    next_id: AtomicU64,
}
#[derive(Clone)]
pub struct EthereumRpc(Arc<Inner>);
impl EthereumRpc {
    pub fn new(config: Config) -> Result<Self> {
        ensure!(
            config.max_rpc_concurrency > 0
                && config.max_rpc_concurrency <= Semaphore::MAX_PERMITS
                && config.max_rpc_requests > 0
                && config.max_rpc_response_bytes > 0
                && config.max_read_bytes > 0
                && config.max_implementations > 0
                && !config.rpc_timeout.is_zero(),
            "Ethereum RPC limits must be positive"
        );
        let url = reqwest::Url::parse(&config.rpc_url)
            .map_err(|_| anyhow::anyhow!("invalid Ethereum RPC URL"))?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.fragment().is_none(),
            "Ethereum RPC URL must be HTTP(S) with a host and no fragment"
        );
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(config.rpc_timeout)
            .user_agent(concat!("arkiv-sourcify-server/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| anyhow::anyhow!("Ethereum HTTP client configuration failed"))?;
        Ok(Self(Arc::new(Inner {
            slots: Semaphore::new(config.max_rpc_concurrency),
            config,
            client,
            next_id: AtomicU64::new(1),
        })))
    }
    pub fn max_implementations(&self) -> usize {
        self.0.config.max_implementations
    }
    /// Pin lazily: non-proxies and fixed EIP-1167 proxies need no Ethereum calls.
    pub fn view(&self) -> EthereumView {
        EthereumView {
            rpc: self.clone(),
            block: None,
            requests: 0,
            bytes: 0,
            fatal: false,
        }
    }
}
struct Block {
    number: String,
    hash: String,
}
pub struct EthereumView {
    rpc: EthereumRpc,
    block: Option<Block>,
    requests: usize,
    bytes: usize,
    fatal: bool,
}
fn data(value: &Value, bytes: Option<usize>) -> Result<String> {
    let text = value
        .as_str()
        .context("Ethereum RPC result is not a hex string")?;
    let hex = text
        .strip_prefix("0x")
        .context("Ethereum RPC result has no hex prefix")?;
    ensure!(
        hex.len().is_multiple_of(2)
            && hex.bytes().all(|b| b.is_ascii_hexdigit())
            && bytes.is_none_or(|bytes| hex.len() == bytes * 2),
        "invalid Ethereum RPC hex result"
    );
    Ok(text.to_owned())
}
fn block(value: &Value) -> Result<Block> {
    let number = value["number"]
        .as_str()
        .context("missing Ethereum block number")?;
    let digits = number
        .strip_prefix("0x")
        .context("invalid Ethereum block number")?;
    ensure!(
        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid Ethereum block number"
    );
    Ok(Block {
        number: number.to_owned(),
        hash: data(&value["hash"], Some(32))?,
    })
}
#[derive(Debug)]
struct RemoteFailure(&'static str);
impl std::fmt::Display for RemoteFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for RemoteFailure {}
impl EthereumView {
    // Preserve upstream Diamond fallback for ordinary remote failures, but latch
    // adapter integrity/resource violations so its catch-all cannot report them
    // as a successfully verified non-proxy.
    fn validate<T>(&mut self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            self.fatal = true;
        }
        result
    }
    async fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        let result = self.rpc_inner(method, params).await;
        if let Err(error) = &result {
            if !error.is::<RemoteFailure>() && !error.is::<crate::proxy::Reverted>() {
                self.fatal = true;
            }
        }
        result
    }
    async fn rpc_inner(&mut self, method: &str, params: Value) -> Result<Value> {
        self.requests += 1;
        let inner = &self.rpc.0;
        ensure!(
            self.requests <= inner.config.max_rpc_requests,
            "Ethereum RPC work limit exceeded"
        );
        let _permit = inner
            .slots
            .acquire()
            .await
            .context("Ethereum RPC gate closed")?;
        let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
        let response = inner
            .client
            .post(&inner.config.rpc_url)
            .header("Accept-Encoding", "identity")
            .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .send()
            .await
            .map_err(|error| -> anyhow::Error {
                if error.is_timeout() {
                    anyhow::anyhow!("Ethereum RPC timeout")
                } else {
                    RemoteFailure("Ethereum RPC transport failed").into()
                }
            })?;
        ensure!(
            response.status().is_success(),
            RemoteFailure("Ethereum RPC HTTP failure")
        );
        ensure!(
            response
                .content_length()
                .is_none_or(|n| n <= inner.config.max_rpc_response_bytes as u64),
            "Ethereum RPC response limit exceeded"
        );
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| -> anyhow::Error {
                if error.is_timeout() {
                    anyhow::anyhow!("Ethereum RPC timeout")
                } else {
                    RemoteFailure("Ethereum RPC body failed").into()
                }
            })?;
            ensure!(
                body.len().saturating_add(chunk.len()) <= inner.config.max_rpc_response_bytes
                    && self.bytes.saturating_add(chunk.len()) <= inner.config.max_read_bytes,
                "Ethereum RPC byte limit exceeded"
            );
            self.bytes += chunk.len();
            body.extend_from_slice(&chunk);
        }
        let value: Value =
            serde_json::from_slice(&body).context("invalid Ethereum JSON-RPC response")?;
        ensure!(
            value["jsonrpc"] == "2.0" && value["id"] == id,
            "Ethereum JSON-RPC response ID/version mismatch"
        );
        ensure!(
            value.get("error").is_some() != value.get("result").is_some(),
            "Ethereum JSON-RPC must contain exactly one result or error"
        );
        if let Some(error) = value.get("error") {
            ensure!(
                error.is_object()
                    && error["code"].as_i64().is_some()
                    && error["message"].is_string(),
                "invalid Ethereum JSON-RPC error envelope"
            );
            if method == "eth_call"
                && error["message"]
                    .as_str()
                    .is_some_and(|s| s.to_ascii_lowercase().contains("revert"))
            {
                return Err(crate::proxy::Reverted.into());
            }
            return Err(RemoteFailure("Ethereum RPC failure").into());
        }
        ensure!(
            value.get("result").is_some(),
            "Ethereum {method} missing RPC result"
        );
        Ok(value["result"].clone())
    }
    async fn tag(&mut self) -> Result<Value> {
        if self.block.is_none() {
            let chain = self.rpc("eth_chainId", json!([])).await?;
            if !chain.as_str().is_some_and(|v| {
                v.strip_prefix("0x")
                    .and_then(|n| u64::from_str_radix(n, 16).ok())
                    == Some(1)
            }) {
                self.fatal = true;
                anyhow::bail!("Ethereum RPC chain must be Mainnet 1");
            }
            let latest = self
                .rpc("eth_getBlockByNumber", json!(["latest", false]))
                .await?;
            self.block = Some(self.validate(block(&latest))?);
        }
        Ok(json!({"blockHash":self.block.as_ref().unwrap().hash,"requireCanonical":true}))
    }
    pub async fn finish(&mut self) -> Result<()> {
        ensure!(
            !self.fatal,
            "Ethereum RPC invariant failed during proxy resolution"
        );
        if let Some(pinned) = &self.block {
            let number = pinned.number.clone();
            let hash = pinned.hash.clone();
            let current = block(
                &self
                    .rpc("eth_getBlockByNumber", json!([number, false]))
                    .await?,
            )?;
            ensure!(
                current.number == number && current.hash.eq_ignore_ascii_case(&hash),
                "Ethereum block changed during proxy resolution"
            );
        }
        Ok(())
    }
}
impl Provider for EthereumView {
    fn get_storage_at<'a>(&'a mut self, address: &'a str, slot: &'a str) -> ReadFuture<'a, String> {
        Box::pin(async move {
            let tag = self.tag().await?;
            let value = self
                .rpc("eth_getStorageAt", json!([address, slot, tag]))
                .await?;
            self.validate(data(&value, Some(32)))
        })
    }
    fn call<'a>(&'a mut self, address: &'a str, calldata: &'a str) -> ReadFuture<'a, String> {
        Box::pin(async move {
            let tag = self.tag().await?;
            let value = self
                .rpc("eth_call", json!([{"to":address,"data":calldata}, tag]))
                .await?;
            self.validate(data(&value, None))
        })
    }
}
