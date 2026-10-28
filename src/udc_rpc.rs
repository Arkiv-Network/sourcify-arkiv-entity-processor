//! Reads one contract of a v4 (UDC) dataset over JSON-RPC (`read-arkiv --format udc`).
//!
//! The contract by owner, schema, chain and address (limit 2), then its dependencies by key, at
//! most [`KEY_BATCH`] keys per `arkiv_query`, in the rounds of [`Reader`]. Two read views:
//!
//! - **pinned** (our node, which keeps history): every query carries one `atBlock` and must answer
//!   at that block; the block's hash is checked again at the end.
//! - **head** (Jalebi, which serves only its head): no `atBlock`. The contract comes from one
//!   query; its dependencies are never changed once written (only contracts are patched) and are
//!   checked by content hash, so the read stays consistent while the head moves.
//!
//! `auto` asks for the block before the head: a node that answers keeps history (pinned at the
//! head), one that refuses with "not retained" serves the head only.
use crate::udc::{
    contract_query, key_query, query_entities, select_json, Reader, Select, UdcEntity, Val,
    KEY_BATCH,
};
use crate::udc_page::{self, Row, Window};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// How queries see the chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    /// Pinned on a node that keeps history, head-only otherwise.
    Auto,
    /// Every query at one block: the given one, or the head when the read starts.
    Pinned(Option<u64>),
    /// Every query at the node's head.
    Head,
}

pub struct UdcReadConfig {
    pub rpc_url: String,
    /// Optional `X-API-KEY` header (Jalebi's API keys); never logged.
    pub api_key: Option<String>,
    pub owner: [u8; 20],
    pub arkiv_chain_id: u64,
    pub prefix: String,
    pub view: View,
    /// Top-level fields to hydrate (minimal fields always); None reads the whole contract.
    pub fields: Option<Vec<String>>,
    pub timeout: Duration,
    pub deadline: Duration,
    pub max_rpc_requests: u64,
    pub max_rpc_response_bytes: u64,
}

pub struct UdcReadResult {
    pub contract: Value,
    /// `pinned` or `head`.
    pub view: &'static str,
    /// The pinned block, or the lowest and highest block the head queries answered at.
    pub blocks: (u64, u64),
    pub rpc_requests: u64,
    pub queries: u64,
    pub entities: u64,
    pub response_bytes: u64,
    pub elapsed: Duration,
}

struct Rpc {
    config: UdcReadConfig,
    agent: ureq::Agent,
    started: Instant,
    requests: u64,
    queries: u64,
    entities: u64,
    response_bytes: u64,
    at: Option<u64>,
    blocks: Option<(u64, u64)>,
}

fn quantity(value: &Value) -> Result<u64> {
    let digits = value
        .as_str()
        .and_then(|s| s.strip_prefix("0x"))
        .context("RPC quantity must be a 0x string")?;
    ensure!(!digits.is_empty(), "empty RPC quantity");
    Ok(u64::from_str_radix(digits, 16)?)
}

/// Attempts per call while the node answers ServerBusy.
const BUSY_ATTEMPTS: u32 = 8;
fn is_busy(e: &anyhow::Error) -> bool {
    e.downcast_ref::<RpcError>()
        .is_some_and(|r| r.code == -32010)
}
/// An RPC error as the node returned it (code and message), distinct from transport failures.
#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}
impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RPC error {}: {}", self.code, self.message)
    }
}
impl std::error::Error for RpcError {}

impl Rpc {
    fn new(config: UdcReadConfig) -> Result<Self> {
        ensure!(
            config.arkiv_chain_id > 0
                && !config.timeout.is_zero()
                && !config.deadline.is_zero()
                && config.max_rpc_requests > 0
                && config.max_rpc_response_bytes > 0,
            "read limits must be positive"
        );
        crate::udc::check_prefix(&config.prefix)?;
        let uri: ureq::http::Uri = config.rpc_url.parse().context("invalid RPC URL")?;
        ensure!(
            matches!(uri.scheme_str(), Some("http" | "https"))
                && uri.host().is_some()
                && !uri.authority().is_some_and(|v| v.as_str().contains('@'))
                && uri.query().is_none()
                && !config.rpc_url.contains('#'),
            "RPC URL must be HTTP(S) without credentials, query, or fragment"
        );
        let agent = ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .user_agent(concat!(
                "arkiv-entity-processor/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .into();
        Ok(Self {
            config,
            agent,
            started: Instant::now(),
            requests: 0,
            queries: 0,
            entities: 0,
            response_bytes: 0,
            at: None,
            blocks: None,
        })
    }
    /// One JSON-RPC call; while the node answers ServerBusy (HTTP 429 or code -32010, which our
    /// node does with about eight requests in flight) it is asked again after a backoff.
    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        for attempt in 1.. {
            match self.call_once(method, params.clone()) {
                Err(e) if attempt < BUSY_ATTEMPTS && is_busy(&e) => {
                    std::thread::sleep(Duration::from_millis(100 << attempt.min(4)));
                }
                other => return other,
            }
        }
        unreachable!()
    }
    fn call_once(&mut self, method: &str, params: Value) -> Result<Value> {
        ensure!(
            self.requests < self.config.max_rpc_requests,
            "RPC request budget exhausted"
        );
        let remaining = self
            .config
            .deadline
            .checked_sub(self.started.elapsed())
            .filter(|v| !v.is_zero())
            .context("Arkiv read deadline exceeded")?;
        self.requests += 1;
        let id = self.requests;
        let body =
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        let mut request = self
            .agent
            .post(&self.config.rpc_url)
            .config()
            .timeout_global(Some(self.config.timeout.min(remaining)))
            .build()
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("Accept-Encoding", "identity");
        if let Some(key) = &self.config.api_key {
            request = request.header("X-API-KEY", key);
        }
        let mut response = request
            .send(body.as_slice())
            .with_context(|| format!("Arkiv RPC {method} transport failed"))?;
        if response.status().as_u16() == 429 {
            return Err(RpcError {
                code: -32010,
                message: "ServerBusy (HTTP 429)".into(),
            }
            .into());
        }
        ensure!(
            response.status().as_u16() == 200,
            "Arkiv RPC {method}: HTTP {}",
            response.status()
        );
        let bytes = response
            .body_mut()
            .with_config()
            .limit(self.config.max_rpc_response_bytes)
            .read_to_vec()
            .with_context(|| {
                format!("Arkiv RPC {method} body failed or exceeded response limit")
            })?;
        self.response_bytes += bytes.len() as u64;
        let envelope: Value =
            serde_json::from_slice(&bytes).context("invalid RPC response JSON")?;
        ensure!(
            envelope.is_object() && envelope["jsonrpc"] == "2.0" && envelope["id"] == id,
            "invalid RPC envelope or response ID"
        );
        ensure!(
            envelope.get("result").is_some() != envelope.get("error").is_some(),
            "invalid RPC result/error envelope"
        );
        if let Some(error) = envelope.get("error") {
            return Err(RpcError {
                code: error["code"].as_i64().unwrap_or(0),
                message: error["message"].as_str().unwrap_or("").to_owned(),
            }
            .into());
        }
        Ok(envelope["result"].clone())
    }
    /// One `arkiv_query` in the read view; returns its entities.
    fn query(&mut self, query: &str, select: &Select, limit: usize) -> Result<Vec<UdcEntity>> {
        self.queries += 1;
        let mut options = json!({"select": select_json(select), "limit": format!("0x{limit:x}")});
        if let Some(at) = self.at {
            options["atBlock"] = json!(format!("0x{at:x}"));
        }
        let result = self.call("arkiv_query", json!([query, options]))?;
        let block = quantity(&result["blockNumber"])?;
        match self.at {
            Some(at) => ensure!(block == at, "query answered at block {block}, not {at}"),
            None => {
                let (low, high) = self.blocks.unwrap_or((block, block));
                self.blocks = Some((low.min(block), high.max(block)));
            }
        }
        let entities = query_entities(&result)?;
        self.entities += entities.len() as u64;
        Ok(entities)
    }
    /// Entities by key, in batches of at most [`KEY_BATCH`]; every key must be found.
    fn by_keys(&mut self, keys: &[[u8; 32]], select: &Select) -> Result<Vec<UdcEntity>> {
        let mut out = Vec::with_capacity(keys.len());
        for batch in keys.chunks(KEY_BATCH) {
            let found = self.query(&key_query(batch), select, batch.len())?;
            for key in batch {
                ensure!(
                    found.iter().any(|e| &e.key == key),
                    "missing dependency {}",
                    crate::hx(key)
                );
            }
            out.extend(found);
        }
        Ok(out)
    }
    fn block_hash(&mut self, number: u64) -> Result<String> {
        let block = self.call(
            "eth_getBlockByNumber",
            json!([format!("0x{number:x}"), false]),
        )?;
        ensure!(!block.is_null(), "block {number} is unavailable");
        ensure!(
            quantity(&block["number"])? == number,
            "RPC returned a different block"
        );
        Ok(block["hash"]
            .as_str()
            .context("block hash")?
            .to_ascii_lowercase())
    }
}

/// Whether the node serves past state: a query at the block before the head either answers
/// (archive) or fails with "not retained" (head only). Any other failure is an error.
fn keeps_history(rpc: &mut Rpc, head: u64) -> Result<bool> {
    if head == 0 {
        return Ok(true);
    }
    let probe = rpc.call(
        "arkiv_query",
        json!([format!("$key = key(0x{})", "00".repeat(32)), {"limit": "0x1", "atBlock": format!("0x{:x}", head - 1)}]),
    );
    match probe {
        Ok(_) => Ok(true),
        Err(e) => match e.downcast_ref::<RpcError>() {
            Some(r) if r.code == -32006 || r.message.contains("not retained") => Ok(false),
            _ => Err(e),
        },
    }
}

/// Reads and hydrates the contract at `address` (0x hex) from a v4 dataset.
pub fn read_contract(config: UdcReadConfig, address: &str) -> Result<UdcReadResult> {
    let address: [u8; 20] = {
        ensure!(
            address.starts_with("0x") && address.len() == 42,
            "invalid address"
        );
        hex::decode(&address[2..])?
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid address"))?
    };
    let owner = config.owner;
    ensure!(owner != [0; 20], "owner must be nonzero");
    let prefix = config.prefix.clone();
    let fields = config.fields.clone();
    let view = config.view;
    let mut rpc = Rpc::new(config)?;
    let chain = quantity(&rpc.call("eth_chainId", json!([]))?)?;
    ensure!(
        chain == rpc.config.arkiv_chain_id,
        "Arkiv chain ID mismatch"
    );
    let head = quantity(&rpc.call("eth_blockNumber", json!([]))?)?;
    let pinned = match view {
        View::Pinned(block) => Some(block.unwrap_or(head)),
        View::Head => None,
        View::Auto => keeps_history(&mut rpc, head)?.then_some(head),
    };
    rpc.at = pinned;
    let pinned_hash = pinned.map(|b| rpc.block_hash(b)).transpose()?;
    let found = rpc.query(&contract_query(&prefix, &owner, &address), &Select::All, 2)?;
    let contract = match found.as_slice() {
        [] => bail!("contract not found in the configured dataset at this view"),
        [one] => one.clone(),
        _ => bail!("ambiguous contract: two entities for one address"),
    };
    let fields: Option<Vec<&str>> = fields
        .as_ref()
        .map(|f| f.iter().map(String::as_str).collect());
    let record = {
        let mut fetch = |keys: &[[u8; 32]], select: &Select| rpc.by_keys(keys, select);
        Reader::batched(&prefix, Some(owner), &mut fetch).hydrate(&contract, fields.as_deref())?
    };
    ensure!(
        record["address"]
            .as_str()
            .is_some_and(|a| a.eq_ignore_ascii_case(&crate::hx(address))),
        "hydrated address mismatch"
    );
    if let (Some(block), Some(hash)) = (pinned, pinned_hash) {
        ensure!(
            rpc.block_hash(block)? == hash,
            "pinned block changed during the read"
        );
    }
    Ok(UdcReadResult {
        contract: record,
        view: if pinned.is_some() { "pinned" } else { "head" },
        blocks: pinned
            .map(|b| (b, b))
            .or(rpc.blocks)
            .unwrap_or((head, head)),
        rpc_requests: rpc.requests,
        queries: rpc.queries,
        entities: rpc.entities,
        response_bytes: rpc.response_bytes,
        elapsed: rpc.started.elapsed(),
    })
}

/// One page of the dataset's contracts in `matchid` order ([`udc_page`]), optionally narrowed by
/// positive clauses (`filter`, the subset both nodes accept). `cursor` is the previous page's last
/// `matchid`; rows are (matchid, address, key).
pub struct PageRequest {
    pub filter: Option<String>,
    pub descending: bool,
    pub cursor: Option<u64>,
    pub size: usize,
    pub limit: usize,
    pub width: u64,
}
pub struct PageResult {
    pub rows: Vec<(u64, String, [u8; 32])>,
    pub next: Option<u64>,
    pub width: u64,
    pub queries: usize,
    pub view: &'static str,
    pub blocks: (u64, u64),
}
pub fn page_contracts(config: UdcReadConfig, request: PageRequest) -> Result<PageResult> {
    let owner = config.owner;
    let prefix = config.prefix.clone();
    let view = config.view;
    let mut rpc = Rpc::new(config)?;
    let chain = quantity(&rpc.call("eth_chainId", json!([]))?)?;
    ensure!(
        chain == rpc.config.arkiv_chain_id,
        "Arkiv chain ID mismatch"
    );
    let head = quantity(&rpc.call("eth_blockNumber", json!([]))?)?;
    rpc.at = match view {
        View::Pinned(block) => Some(block.unwrap_or(head)),
        View::Head => None,
        View::Auto => keeps_history(&mut rpc, head)?.then_some(head),
    };
    let scope = format!(
        "$owner = addr({}) AND arkiv.schema = str('{prefix}.contract') AND chainid = u64(1)",
        crate::hx(owner)
    );
    let mut addresses = std::collections::HashMap::new();
    let mut query = |window: Window, limit: usize| -> Result<(Vec<Row>, bool)> {
        let mut text = scope.clone();
        if let Some(f) = &request.filter {
            text.push_str(&format!(" AND {f}"));
        }
        let clauses = window.clauses();
        if !clauses.is_empty() {
            text.push_str(&format!(" AND {clauses}"));
        }
        rpc.queries += 1;
        let mut options = json!({"select": select_json(&Select::Named(vec!["matchid".into(), "address".into()])), "limit": format!("0x{limit:x}")});
        if let Some(at) = rpc.at {
            options["atBlock"] = json!(format!("0x{at:x}"));
        }
        let result = rpc.call("arkiv_query", json!([text, options]))?;
        let block = quantity(&result["blockNumber"])?;
        match rpc.at {
            Some(at) => ensure!(block == at, "query answered at block {block}, not {at}"),
            None => {
                let (low, high) = rpc.blocks.unwrap_or((block, block));
                rpc.blocks = Some((low.min(block), high.max(block)));
            }
        }
        let data = result["data"]
            .as_array()
            .context("query result has no data")?;
        let complete = data.len() < limit || result.get("cursor").is_none_or(Value::is_null);
        let mut rows = vec![];
        for row in data {
            let e = UdcEntity::from_json(row)?;
            let Some(Val::U64(matchid)) = e.get("matchid") else {
                bail!("contract without matchid")
            };
            if let Some(Val::Addr(a)) = e.get("address") {
                addresses.insert(e.key, crate::hx(a));
            }
            rows.push(Row {
                matchid: *matchid,
                key: e.key,
            });
        }
        Ok((rows, complete))
    };
    let page = if request.descending {
        udc_page::descending(
            &mut query,
            request.cursor,
            request.size,
            request.limit,
            request.width,
        )?
    } else {
        udc_page::ascending(
            &mut query,
            request.cursor,
            request.size,
            request.limit,
            request.width,
        )?
    };
    Ok(PageResult {
        rows: page
            .rows
            .iter()
            .map(|r| {
                (
                    r.matchid,
                    addresses.get(&r.key).cloned().unwrap_or_default(),
                    r.key,
                )
            })
            .collect(),
        next: page.next,
        width: page.width,
        queries: page.queries,
        view: if rpc.at.is_some() { "pinned" } else { "head" },
        blocks: rpc
            .at
            .map(|b| (b, b))
            .or(rpc.blocks)
            .unwrap_or((head, head)),
    })
}
