//! Read-only HTTP JSON-RPC transport for v4 (UDC) entities (docs/udc-proposal.md).
//!
//! The contract by owner, `arkiv.schema`, chain and address (limit 2), then dependencies by key,
//! at most [`KEY_BATCH`] keys per `arkiv_query` (our node's planned path), with the attribute
//! selection `udc::Reader` asks for. Two read views:
//!
//! - **pinned**: a node that keeps history (our node). Each request pins the head when it starts;
//!   every query carries that `atBlock` and must answer at it, and the block's hash is checked
//!   again before responding.
//! - **head**: a node that serves only its head (Jalebi). No `atBlock`. The contract comes from
//!   one query; its dependencies never change once written (only contracts are patched) and are
//!   checked by content hash, so a read stays consistent while the head moves.
//!
//! `auto` decides once at startup: a query 64 blocks behind the head either answers (pinned)
//! or is refused as not retained (head).
use super::transport::{check_url, Budget, Limits as TransportLimits, Transport};
use crate::backend::{
    Backend, BackendError, ReadFuture, ReadSession, UdcBackend as UdcApi, UdcSession,
};
use anyhow::{bail, ensure, Context, Result};
use arkiv_entity_processor::{
    hx,
    udc::{
        check_prefix, contract_query, key_query, query_entities, select_json, Select, UdcEntity,
        Val, KEY_BATCH,
    },
};
use futures_util::{stream, StreamExt, TryStreamExt};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

/// Attempts after a busy answer (HTTP 429 or `-32010 ServerBusy`): our node sheds load when about
/// 8 to 12 requests are in flight. Waits 100, 200, 400, 800 and 1,600 ms, within the request's
/// deadline; any other failure is final.
pub const BUSY_RETRIES: u32 = 5;
/// How far behind the head the `auto` view asks.
pub const HISTORY_PROBE_DEPTH: u64 = 64;
/// Parallel RPC calls of the whole server, below the load at which our node answers busy.
pub const DEFAULT_CONCURRENCY: usize = 4;
/// How queries see the chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    /// Pinned on a node that keeps history, head-only otherwise (decided at startup).
    Auto,
    /// Every query of a request at the block that was the head when it started.
    Pinned,
    /// Every query at the node's head.
    Head,
}

#[derive(Clone)]
pub struct Config {
    pub rpc_url: String,
    /// Sent as `X-API-KEY` (Jalebi's API keys); never logged or reported.
    pub api_key: Option<String>,
    pub owner: String,
    pub arkiv_chain_id: u64,
    /// `arkiv.schema` prefix: entities are `<prefix>.<kind>`.
    pub prefix: String,
    pub view: View,
    /// Optional hash of block 0: the chain's identity, checked at the start of every read.
    pub genesis_hash: Option<String>,
    pub rpc_timeout: Duration,
    pub max_rpc_concurrency: usize,
    pub max_rpc_requests: usize,
    pub max_rpc_response_bytes: usize,
    /// Cumulative RPC input budget per request; independent of HTTP response size.
    pub max_read_bytes: usize,
}
impl std::fmt::Debug for Config {
    /// The API key is never printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("rpc_url", &self.rpc_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("owner", &self.owner)
            .field("arkiv_chain_id", &self.arkiv_chain_id)
            .field("prefix", &self.prefix)
            .field("view", &self.view)
            .field("genesis_hash", &self.genesis_hash)
            .finish_non_exhaustive()
    }
}
impl Config {
    pub fn new(rpc_url: String, owner: String, arkiv_chain_id: u64) -> Self {
        Self {
            rpc_url,
            api_key: None,
            owner,
            arkiv_chain_id,
            prefix: "sourcify.v4".into(),
            view: View::Auto,
            genesis_hash: None,
            rpc_timeout: Duration::from_secs(15),
            max_rpc_concurrency: DEFAULT_CONCURRENCY,
            max_rpc_requests: 100_000,
            max_rpc_response_bytes: 8 * 1024 * 1024,
            max_read_bytes: 256 * 1024 * 1024,
        }
    }
}

struct Inner {
    config: Config,
    owner: [u8; 20],
    /// The resolved view: pinned or head.
    pinned: bool,
    transport: Transport,
}
#[derive(Clone)]
pub struct UdcBackend(Arc<Inner>);

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
fn corrupt(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<BackendError>().is_some() {
        error
    } else {
        BackendError::Corrupt(format!("{error:#}")).into()
    }
}
#[derive(Clone, Debug, PartialEq)]
struct Block {
    number: u64,
    hash: String,
}

impl UdcBackend {
    pub async fn connect(mut config: Config) -> Result<Self> {
        config.owner = hex_value(&config.owner, 20)?;
        ensure!(
            config.owner != format!("0x{}", "0".repeat(40)),
            "owner must be nonzero"
        );
        ensure!(config.arkiv_chain_id > 0, "Arkiv chain ID must be positive");
        check_prefix(&config.prefix)?;
        check_url(&config.rpc_url)?;
        if let Some(hash) = &config.genesis_hash {
            config.genesis_hash = Some(hex_value(hash, 32)?);
        }
        let owner: [u8; 20] = hex::decode(&config.owner[2..])?
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid owner"))?;
        let transport = Transport::new(
            config.rpc_url.clone(),
            config.api_key.clone(),
            BUSY_RETRIES,
            TransportLimits {
                rpc_timeout: config.rpc_timeout,
                max_rpc_concurrency: config.max_rpc_concurrency,
                max_rpc_requests: config.max_rpc_requests,
                max_rpc_response_bytes: config.max_rpc_response_bytes,
                max_read_bytes: config.max_read_bytes,
            },
        )?;
        let mut inner = Inner {
            pinned: config.view != View::Head,
            config,
            owner,
            transport,
        };
        if inner.config.view == View::Auto {
            inner.pinned = inner.keeps_history().await?;
        }
        let backend = Self(Arc::new(inner));
        backend.begin_udc_read(None).await?;
        Ok(backend)
    }
    /// `pinned` or `head`, as resolved at startup.
    pub fn read_view(&self) -> &'static str {
        if self.0.pinned {
            "pinned"
        } else {
            "head"
        }
    }
}
impl Inner {
    /// Whether the node serves past state: a query well behind the head answers on an archive
    /// node and is refused as not retained by a head-only one (even one whose store or replicas
    /// trail `eth_blockNumber` by a few blocks).
    async fn keeps_history(&self) -> Result<bool> {
        let budget = Budget::default();
        let head = quantity(
            &self
                .transport
                .rpc(&budget, "eth_blockNumber", json!([]))
                .await?,
        )?;
        if head == 0 {
            return Ok(true);
        }
        let probe = self
            .transport
            .call(
                &budget,
                "arkiv_query",
                json!([format!("$key = key(0x{})", "00".repeat(32)), {"limit":"0x1","atBlock":format!("0x{:x}", head.saturating_sub(HISTORY_PROBE_DEPTH))}]),
            )
            .await?;
        match probe {
            Ok(_) => Ok(true),
            Err(error)
                if error["code"] == -32006
                    || error["message"]
                        .as_str()
                        .is_some_and(|m| m.contains("not retained")) =>
            {
                Ok(false)
            }
            Err(_) => bail!(BackendError::Unavailable(
                "the read-view probe returned an RPC error".into()
            )),
        }
    }
    async fn block(&self, budget: &Budget, selector: &str) -> Result<Block> {
        let result = self
            .transport
            .rpc(budget, "eth_getBlockByNumber", json!([selector, false]))
            .await?;
        ensure!(
            !result.is_null(),
            BackendError::Unavailable("requested block is unavailable/pruned".into())
        );
        Ok(Block {
            number: quantity(&result["number"]).map_err(corrupt)?,
            hash: hex_value(result["hash"].as_str().context("missing block hash")?, 32)
                .map_err(corrupt)?,
        })
    }
}

struct Session {
    inner: Arc<Inner>,
    budget: Budget,
    block: Option<Block>,
}
impl Session {
    async fn query(&self, query: &str, select: &Select, limit: usize) -> Result<Vec<UdcEntity>> {
        let mut options = json!({"select": select_json(select), "limit": format!("0x{limit:x}")});
        if let Some(block) = &self.block {
            options["atBlock"] = json!(format!("0x{:x}", block.number));
        }
        let result = self
            .inner
            .transport
            .rpc(&self.budget, "arkiv_query", json!([query, options]))
            .await?;
        if let Some(block) = &self.block {
            ensure!(
                quantity(&result["blockNumber"]).map_err(corrupt)? == block.number,
                BackendError::StateChanged("query returned a different block".into())
            );
        }
        if result.get("cursor").is_some_and(|c| !c.is_null()) {
            bail!(BackendError::Ambiguous(format!(
                "query continues past its limit: {query}"
            )));
        }
        query_entities(&result).map_err(corrupt)
    }
}
impl Backend for UdcBackend {
    fn begin_read(&self) -> ReadFuture<'_, Box<dyn ReadSession>> {
        Box::pin(async {
            Err(BackendError::Unavailable("a v4 backend serves v4 (UDC) reads only".into()).into())
        })
    }
    fn metadata(&self) -> Value {
        let c = &self.0.config;
        json!({"backend":"arkiv","format":"udc","arkivChainId":c.arkiv_chain_id,"sourceChainId":"1",
            "owner":c.owner,"schema":format!("{}.*", c.prefix),"genesisHash":c.genesis_hash,
            "readView":if self.0.pinned {"current state, pinned per request"} else {"head (no history)"},
            "rpcRequests":self.0.transport.counters.requests.load(Ordering::Relaxed),
            "rpcResponseBytes":self.0.transport.counters.bytes.load(Ordering::Relaxed)})
    }
    fn check_ready(&self) -> ReadFuture<'_, ()> {
        Box::pin(async { self.begin_udc_read(None).await.map(|_| ()) })
    }
    fn udc(&self) -> Option<&dyn UdcApi> {
        Some(self)
    }
}
impl UdcApi for UdcBackend {
    fn prefix(&self) -> &str {
        &self.0.config.prefix
    }
    fn owner(&self) -> [u8; 20] {
        self.0.owner
    }
    fn begin_udc_read(
        &self,
        trace: Option<crate::trace::RpcTrace>,
    ) -> ReadFuture<'_, Box<dyn UdcSession>> {
        Box::pin(async move {
            let budget = Budget {
                trace,
                ..Budget::default()
            };
            let inner = &self.0;
            let chain = inner
                .transport
                .rpc(&budget, "eth_chainId", json!([]))
                .await?;
            ensure!(
                quantity(&chain).map_err(corrupt)? == inner.config.arkiv_chain_id,
                BackendError::Unavailable("Arkiv network mismatch".into())
            );
            if let Some(expected) = &inner.config.genesis_hash {
                let genesis = inner.block(&budget, "0x0").await?;
                ensure!(
                    genesis.number == 0 && &genesis.hash == expected,
                    BackendError::Unavailable("genesis block hash mismatch".into())
                );
            }
            let block = match inner.pinned {
                true => Some(inner.block(&budget, "latest").await?),
                false => None,
            };
            Ok(Box::new(Session {
                inner: inner.clone(),
                budget,
                block,
            }) as Box<dyn UdcSession>)
        })
    }
}
impl UdcSession for Session {
    fn find_contract<'a>(&'a mut self, address: &'a str) -> ReadFuture<'a, Option<UdcEntity>> {
        Box::pin(async move {
            let address: [u8; 20] = hex::decode(hex_value(address, 20)?.trim_start_matches("0x"))?
                .try_into()
                .map_err(|_| anyhow::anyhow!("invalid address"))?;
            let query = contract_query(&self.inner.config.prefix, &self.inner.owner, &address);
            let found = self.query(&query, &Select::All, 2).await?;
            match found.as_slice() {
                [] => Ok(None),
                [one] => {
                    ensure!(
                        one.get("address") == Some(&Val::Addr(address)),
                        BackendError::Corrupt("queried contract has another address".into())
                    );
                    Ok(Some(one.clone()))
                }
                _ => Err(
                    BackendError::Ambiguous(format!("two contracts for {}", hx(address))).into(),
                ),
            }
        })
    }
    fn fetch<'a>(
        &'a mut self,
        keys: &'a [[u8; 32]],
        select: &'a Select,
    ) -> ReadFuture<'a, Vec<UdcEntity>> {
        Box::pin(async move {
            let this = &*self;
            let batches: Vec<Vec<[u8; 32]>> = keys.chunks(KEY_BATCH).map(<[_]>::to_vec).collect();
            let batches: Vec<Vec<UdcEntity>> = stream::iter(batches)
                .map(|batch| async move {
                    let found = this.query(&key_query(&batch), select, batch.len()).await?;
                    let wanted: HashSet<&[u8; 32]> = batch.iter().collect();
                    let mut seen = HashSet::new();
                    for e in &found {
                        ensure!(
                            wanted.contains(&e.key) && seen.insert(e.key),
                            BackendError::Corrupt("key query returned another entity".into())
                        );
                    }
                    if let Some(missing) = batch.iter().find(|k| !seen.contains(*k)) {
                        bail!(BackendError::MissingDependency(hx(missing)));
                    }
                    Ok::<_, anyhow::Error>(found)
                })
                .buffer_unordered(this.inner.config.max_rpc_concurrency)
                .try_collect()
                .await?;
            Ok(batches.into_iter().flatten().collect())
        })
    }
    fn finish(&mut self) -> ReadFuture<'_, ()> {
        Box::pin(async {
            if let Some(block) = &self.block {
                let actual = self
                    .inner
                    .block(&self.budget, &format!("0x{:x}", block.number))
                    .await?;
                ensure!(
                    &actual == block,
                    BackendError::StateChanged("pinned block changed during read".into())
                );
            }
            Ok(())
        })
    }
}
