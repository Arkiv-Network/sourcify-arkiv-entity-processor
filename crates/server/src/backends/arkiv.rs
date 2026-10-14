//! Read-only HTTP JSON-RPC transport for the prefill entity format.
//! Every lookup is scoped to owner + project and uses a retained block number.
use crate::backend::{Backend, BackendError, ReadFuture, ReadSession};
use anyhow::{ensure, Context, Result};
use arkiv_entity_processor::{
    arkiv::{decode_wire, rpc_wire},
    Entity,
};
use futures_util::{stream, StreamExt, TryStreamExt};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::Semaphore;

#[derive(Clone, Debug)]
pub struct Config {
    pub rpc_url: String,
    pub owner: String,
    pub arkiv_chain_id: u64,
    /// Trust anchor for the completed imported genesis. Current state remains mutable.
    pub genesis_state_root: String,
    pub rpc_timeout: Duration,
    pub max_rpc_concurrency: usize,
    pub max_rpc_requests: usize,
    pub max_rpc_response_bytes: usize,
    /// Cumulative RPC input budget per request; independent of HTTP response size.
    pub max_read_bytes: usize,
}
impl Config {
    pub fn new(
        rpc_url: String,
        owner: String,
        arkiv_chain_id: u64,
        genesis_state_root: String,
    ) -> Self {
        Self {
            rpc_url,
            owner,
            arkiv_chain_id,
            genesis_state_root,
            rpc_timeout: Duration::from_secs(15),
            max_rpc_concurrency: 16,
            max_rpc_requests: 100_000,
            max_rpc_response_bytes: 8 * 1024 * 1024,
            max_read_bytes: 256 * 1024 * 1024,
        }
    }
}

#[derive(Default)]
struct Counters {
    requests: AtomicU64,
    bytes: AtomicU64,
}
struct Inner {
    config: Config,
    client: reqwest::Client,
    slots: Semaphore,
    next_id: AtomicU64,
    counters: Counters,
}
#[derive(Clone)]
pub struct ArkivBackend(Arc<Inner>);

fn hex_value(value: &str, bytes: usize) -> Result<String> {
    ensure!(
        value.starts_with("0x") && value.len() == bytes * 2 + 2 && hex::decode(&value[2..]).is_ok(),
        "invalid {bytes}-byte hex value"
    );
    Ok(value.to_ascii_lowercase())
}
fn quantity(value: &Value) -> Result<u64> {
    let digits = value
        .as_str()
        .and_then(|s| s.strip_prefix("0x"))
        .context("invalid RPC quantity")?;
    ensure!(!digits.is_empty(), "empty RPC quantity");
    Ok(u64::from_str_radix(digits, 16)?)
}
#[derive(Clone, Debug, PartialEq)]
struct Block {
    number: u64,
    hash: String,
    state_root: String,
}
fn block(value: &Value) -> Result<Block> {
    Ok(Block {
        number: quantity(&value["number"])?,
        hash: hex_value(value["hash"].as_str().context("missing block hash")?, 32)?,
        state_root: hex_value(
            value["stateRoot"].as_str().context("missing state root")?,
            32,
        )?,
    })
}

impl ArkivBackend {
    pub async fn connect(mut config: Config) -> Result<Self> {
        config.owner = hex_value(&config.owner, 20)?;
        config.genesis_state_root = hex_value(&config.genesis_state_root, 32)?;
        ensure!(
            config.owner != format!("0x{}", "0".repeat(40)),
            "owner must be nonzero"
        );
        ensure!(
            config.arkiv_chain_id > 0
                && config.max_rpc_concurrency > 0
                && config.max_rpc_concurrency <= Semaphore::MAX_PERMITS
                && config.max_rpc_requests > 0
                && config.max_rpc_response_bytes > 0
                && config.max_read_bytes > 0
                && !config.rpc_timeout.is_zero(),
            "RPC limits must be positive"
        );
        let url = reqwest::Url::parse(&config.rpc_url).context("invalid RPC URL")?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "RPC URL must be HTTP(S) without credentials, query or fragment"
        );
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(config.rpc_timeout)
            .user_agent(concat!("arkiv-sourcify-server/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let backend = Self(Arc::new(Inner {
            slots: Semaphore::new(config.max_rpc_concurrency),
            config,
            client,
            next_id: AtomicU64::new(1),
            counters: Counters::default(),
        }));
        backend.begin_read().await?;
        Ok(backend)
    }
}
#[derive(Default)]
struct Budget {
    trace: Option<crate::trace::RpcTrace>,
    requests: AtomicU64,
    bytes: AtomicU64,
}
impl Inner {
    async fn rpc(&self, budget: &Budget, method: &str, params: Value) -> Result<Value> {
        ensure!(
            budget.requests.fetch_add(1, Ordering::Relaxed) < self.config.max_rpc_requests as u64,
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
            let response = self
                .client
                .post(&self.config.rpc_url)
                .header("Accept-Encoding", "identity")
                .header("Accept", "application/json")
                .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .send()
                .await
                .map_err(|_| BackendError::Unavailable(format!("{method} transport failed")))?;
            ensure!(
                response.status().is_success(),
                BackendError::Unavailable(format!("{method}: HTTP {}", response.status()))
            );
            ensure!(
                response
                    .content_length()
                    .is_none_or(|n| n <= self.config.max_rpc_response_bytes as u64),
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
                            .config
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
                        <= self.config.max_read_bytes as u64,
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
            ensure!(
                envelope.get("error").is_none(),
                BackendError::Unavailable(format!("{method} returned RPC error"))
            );
            Ok(envelope["result"].clone())
        }
        .await;
        if let Some(call) = call {
            call.finish(result.is_ok());
        }
        result
    }
    async fn read_block(&self, budget: &Budget, selector: &str) -> Result<Block> {
        let result = self
            .rpc(budget, "eth_getBlockByNumber", json!([selector, false]))
            .await?;
        ensure!(
            !result.is_null(),
            BackendError::Unavailable("requested block is unavailable/pruned".into())
        );
        block(&result)
    }
    async fn wire(&self, budget: &Budget, key: &str, number: u64) -> Result<Value> {
        let value = self
            .rpc(budget, "arkiv_getEntity", json!([key, number]))
            .await?;
        ensure!(
            !value.is_null(),
            BackendError::MissingDependency(key.to_owned())
        );
        rpc_wire(&value, key, &self.config.owner)
    }
}
struct Session {
    inner: Arc<Inner>,
    budget: Budget,
    block: Block,
    wires: HashMap<String, Value>,
}
impl Backend for ArkivBackend {
    fn begin_read(&self) -> ReadFuture<'_, Box<dyn ReadSession>> {
        self.begin_read_with_trace(None)
    }
    fn begin_read_with_trace(
        &self,
        trace: Option<crate::trace::RpcTrace>,
    ) -> ReadFuture<'_, Box<dyn ReadSession>> {
        Box::pin(async move {
            let budget = Budget {
                trace,
                ..Budget::default()
            };
            let chain = self.0.rpc(&budget, "eth_chainId", json!([])).await?;
            ensure!(
                quantity(&chain)? == self.0.config.arkiv_chain_id,
                BackendError::Unavailable("Arkiv network mismatch".into())
            );
            let genesis = self.0.read_block(&budget, "0x0").await?;
            ensure!(
                genesis.number == 0 && genesis.state_root == self.0.config.genesis_state_root,
                BackendError::Unavailable("imported genesis state root mismatch".into())
            );
            let block = self.0.read_block(&budget, "latest").await?;
            Ok(Box::new(Session {
                inner: self.0.clone(),
                budget,
                block,
                wires: HashMap::new(),
            }) as Box<dyn ReadSession>)
        })
    }
    fn metadata(&self) -> Value {
        json!({"backend":"arkiv","arkivChainId":self.0.config.arkiv_chain_id,"sourceChainId":"1","owner":self.0.config.owner,"genesisStateRoot":self.0.config.genesis_state_root,"readView":"current state, pinned per request","schema":"sourcify.verified-contract.v3","rpcRequests":self.0.counters.requests.load(Ordering::Relaxed),"rpcResponseBytes":self.0.counters.bytes.load(Ordering::Relaxed)})
    }
}
impl Session {
    async fn query_one(&self, selector: &str) -> Result<Option<String>> {
        let query = format!(
            "projectid = str('sourcify') AND $owner = addr({}) AND {selector}",
            self.inner.config.owner
        );
        let result=self.inner.rpc(&self.budget,"arkiv_query",json!([query,{"atBlock":format!("0x{:x}",self.block.number),"limit":"0x2","select":{"key":true}}])).await?;
        ensure!(
            quantity(&result["blockNumber"])? == self.block.number,
            BackendError::StateChanged("query returned a different block".into())
        );
        let data = result["data"]
            .as_array()
            .context("query response has no data array")?;
        ensure!(
            data.len() <= 1 && result.get("cursor").is_none_or(Value::is_null),
            BackendError::Ambiguous(selector.into())
        );
        data.first()
            .map(|v| hex_value(v["key"].as_str().context("query result missing key")?, 32))
            .transpose()
    }
    async fn entity(&mut self, key: &str) -> Result<Entity> {
        if !self.wires.contains_key(key) {
            self.wires.insert(
                key.into(),
                self.inner
                    .wire(&self.budget, key, self.block.number)
                    .await?,
            );
        }
        let wire = self.wires.get(key).unwrap().clone();
        let mut references = wire
            .as_object()
            .context("invalid normalized wire")?
            .values()
            .filter_map(|v| v.as_str()?.strip_prefix("key:"))
            .map(str::to_owned)
            .filter(|key| !self.wires.contains_key(key))
            .collect::<Vec<_>>();
        references.sort_unstable();
        references.dedup();
        let inner = &self.inner;
        let budget = &self.budget;
        let number = self.block.number;
        let wires: Vec<_> = stream::iter(references)
            .map(|key| async move {
                let wire = inner.wire(budget, &key, number).await?;
                Ok::<_, anyhow::Error>((key, wire))
            })
            .buffer_unordered(inner.config.max_rpc_concurrency)
            .try_collect()
            .await?;
        self.wires.extend(wires);
        decode_wire(&wire, |reference| {
            let staging = self
                .wires
                .get(reference)
                .and_then(|v| v["stagingid"].as_str())
                .and_then(|s| s.strip_prefix("bytes32:0x"))
                .context("unresolved staging reference")?;
            hex::decode(staging)?
                .try_into()
                .map_err(|_| anyhow::anyhow!("invalid staging identity"))
        })
    }
}
impl ReadSession for Session {
    fn find_contract<'a>(
        &'a mut self,
        chain: &'a str,
        address: &'a str,
    ) -> ReadFuture<'a, Option<Entity>> {
        Box::pin(async move {
            ensure!(chain == "1", "unsupported source chain");
            let address = hex_value(address, 20)?;
            let selector=format!("kind = str('verified_contract') AND chainid = u64(1) AND address = addr({address})");
            let Some(key) = self.query_one(&selector).await? else {
                return Ok(None);
            };
            let entity = self.entity(&key).await?;
            ensure!(
                entity.kind == "verified_contract"
                    && entity.attributes["chainid"]["value"] == "1"
                    && entity.attributes["address"]["value"]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case(&address)),
                "queried contract attributes differ from request"
            );
            Ok(Some(entity))
        })
    }
    fn resolve<'a>(
        &'a mut self,
        kind: &'a str,
        hash: &'a str,
        part: Option<u64>,
    ) -> ReadFuture<'a, Entity> {
        Box::pin(async move {
            ensure!(
                matches!(kind, "sourcefile" | "compilation" | "code" | "blob"),
                "unsupported dependency kind"
            );
            let hash = hex_value(hash, 32)?;
            let mut selector = format!("kind = str('{kind}') AND hash = bytes32({hash})");
            if let Some(part) = part {
                ensure!(part <= i32::MAX as u64, "invalid blob part");
                selector.push_str(&format!(" AND part = i32({part})"));
            }
            let key = self
                .query_one(&selector)
                .await?
                .ok_or(BackendError::MissingDependency(selector))?;
            let entity = self.entity(&key).await?;
            ensure!(
                entity.kind == kind
                    && entity.attributes["hash"]["value"] == hash
                    && part.is_none_or(|p| entity.attributes["part"]["value"] == p),
                "queried dependency attributes differ from locator"
            );
            Ok(entity)
        })
    }
    fn finish(&mut self) -> ReadFuture<'_, ()> {
        Box::pin(async {
            let actual = self
                .inner
                .read_block(&self.budget, &format!("0x{:x}", self.block.number))
                .await?;
            ensure!(
                actual == self.block,
                BackendError::StateChanged("pinned block changed during read".into())
            );
            Ok(())
        })
    }
}
