//! First read path for the processor's v3 entities imported by arkiv-prefill.
//! Reads only RPC data at one explicit block; no bundle or live Sourcify fallback.
use crate::{checksum_address, entity_key, hydrate, wire, Entity, BUDGET};
use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Map, Value};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

pub struct ReadConfig {
    pub rpc_url: String,
    pub owner: String,
    pub arkiv_chain_id: u64,
    pub block: u64,
    pub timeout: Duration,
    pub deadline: Duration,
    pub max_rpc_requests: u64,
    /// Bounds each RPC envelope, not the assembled contract response.
    pub max_rpc_response_bytes: u64,
}

pub struct ReadResult {
    pub contract: Value,
    pub block: u64,
    pub block_hash: String,
    pub rpc_requests: u64,
    pub entity_reads: u64,
    pub queries: u64,
    pub response_bytes: u64,
    pub elapsed: Duration,
}

fn hex_value(value: &str, bytes: usize) -> Result<String> {
    ensure!(
        value.starts_with("0x") && value.len() == bytes * 2 + 2 && hex::decode(&value[2..]).is_ok(),
        "invalid {bytes}-byte hex value"
    );
    Ok(value.to_ascii_lowercase())
}
fn quantity(value: &Value) -> Result<u64> {
    let text = value.as_str().context("RPC quantity must be hex string")?;
    let digits = text
        .strip_prefix("0x")
        .context("RPC quantity must start with 0x")?;
    ensure!(!digits.is_empty(), "empty RPC quantity");
    Ok(u64::from_str_radix(digits, 16)?)
}

/// Hydrate a Mainnet contract from a scoped, explicitly pinned Arkiv prefill dataset.
pub fn read_contract(config: ReadConfig, address: &str) -> Result<ReadResult> {
    let address = hex_value(address, 20)?;
    let mut reader = Reader::new(config)?;
    let chain_id = reader.rpc("eth_chainId", json!([]))?;
    ensure!(
        quantity(&chain_id)? == reader.config.arkiv_chain_id,
        "Arkiv chain ID mismatch"
    );
    let block_hash = reader.block_hash()?;
    let selector = format!(
        "kind = str('verified_contract') AND chainid = u64(1) AND address = addr({address})"
    );
    let key = reader
        .query_one(&selector)?
        .context("contract not found in the configured Arkiv namespace at this block")?;
    let entity = reader.entity(&key)?;
    ensure!(
        entity.kind == "verified_contract"
            && entity.attributes["chainid"]["value"] == "1"
            && entity.attributes["address"]["value"]
                .as_str()
                .map(str::to_ascii_lowercase)
                .as_deref()
                == Some(&address),
        "queried contract attributes do not match the requested Mainnet address"
    );
    let contract =
        hydrate::hydrate_with(&entity, |kind, hash, part| reader.resolve(kind, hash, part))?;
    ensure!(
        contract["chainId"] == "1" || contract["chainId"] == 1,
        "hydrated source chain mismatch"
    );
    ensure!(
        contract["address"]
            .as_str()
            .map(str::to_ascii_lowercase)
            .as_deref()
            == Some(&address),
        "hydrated address mismatch"
    );
    // Arkiv's historical API accepts a block number, not an EIP-1898 hash selector.
    ensure!(
        reader.block_hash()? == block_hash,
        "pinned block changed during the read"
    );
    Ok(ReadResult {
        contract,
        block: reader.config.block,
        block_hash,
        rpc_requests: reader.requests,
        entity_reads: reader.entity_reads,
        queries: reader.queries,
        response_bytes: reader.response_bytes,
        elapsed: reader.started.elapsed(),
    })
}

struct Reader {
    config: ReadConfig,
    agent: ureq::Agent,
    started: Instant,
    requests: u64,
    entity_reads: u64,
    queries: u64,
    response_bytes: u64,
    /// Includes validated namespace/type metadata for direct staging-key references.
    wires: HashMap<String, Value>,
}

impl Reader {
    fn new(mut config: ReadConfig) -> Result<Self> {
        config.owner = hex_value(&config.owner, 20)?;
        ensure!(
            config.owner != format!("0x{}", "0".repeat(40)),
            "owner must be nonzero"
        );
        ensure!(
            config.arkiv_chain_id > 0
                && !config.timeout.is_zero()
                && !config.deadline.is_zero()
                && config.max_rpc_requests > 0
                && config.max_rpc_response_bytes > 0,
            "read limits must be positive"
        );
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
            entity_reads: 0,
            queries: 0,
            response_bytes: 0,
            wires: HashMap::new(),
        })
    }

    fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
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
        let body = serde_json::to_vec(
            &json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}),
        )?;
        let mut response = self
            .agent
            .post(&self.config.rpc_url)
            .config()
            .timeout_global(Some(self.config.timeout.min(remaining)))
            .build()
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("Accept-Encoding", "identity")
            .send(body.as_slice())
            .with_context(|| format!("Arkiv RPC {method} transport failed"))?;
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
            bail!("Arkiv RPC {method} returned error: {error}");
        }
        Ok(envelope["result"].clone())
    }

    fn block_hash(&mut self) -> Result<String> {
        let block = self.rpc(
            "eth_getBlockByNumber",
            json!([format!("0x{:x}", self.config.block), false]),
        )?;
        ensure!(
            quantity(&block["number"])? == self.config.block,
            "RPC returned a different block"
        );
        hex_value(wire::string(&block, "hash")?, 32)
    }

    fn query_one(&mut self, selector: &str) -> Result<Option<String>> {
        let query = format!(
            "projectid = str('sourcify') AND $owner = addr({}) AND {selector}",
            self.config.owner
        );
        self.queries += 1;
        let result = self.rpc(
            "arkiv_query",
            json!([query, {
                "atBlock":format!("0x{:x}",self.config.block), "limit":"0x2", "select":{"key":true}
            }]),
        )?;
        ensure!(
            quantity(&result["blockNumber"])? == self.config.block,
            "query returned a different block"
        );
        let data = result["data"]
            .as_array()
            .context("query result has no data array")?;
        ensure!(
            data.len() <= 1 && result.get("cursor").is_none_or(Value::is_null),
            "ambiguous entity selection (multiple records or continuation cursor): {selector}"
        );
        data.first()
            .map(|entry| hex_value(wire::string(entry, "key")?, 32))
            .transpose()
    }

    fn get_wire(&mut self, key: &str) -> Result<Value> {
        let key = hex_value(key, 32)?;
        if let Some(wire) = self.wires.get(&key) {
            return Ok(wire.clone());
        }
        self.entity_reads += 1;
        // This node's getEntity historical parameter is a JSON integer, unlike query.atBlock.
        let value = self.rpc("arkiv_getEntity", json!([key, self.config.block]))?;
        ensure!(
            !value.is_null(),
            "missing dependency entity {key} at pinned block"
        );
        let wire = rpc_wire(&value, &key, &self.config.owner)?;
        self.wires.insert(key, wire.clone());
        Ok(wire)
    }

    fn staging_id(&mut self, key: &str) -> Result<[u8; 32]> {
        let wire = self.get_wire(key)?;
        let value = wire::typed(&wire, "stagingid", "bytes32:0x")?;
        hex::decode(value)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid staging ID"))
    }

    fn entity(&mut self, key: &str) -> Result<Entity> {
        let wire = self.get_wire(key)?;
        crate::wire::decode(&wire, |reference| self.staging_id(reference))
            .with_context(|| format!("corrupt Arkiv entity {key}"))
    }

    fn resolve(&mut self, kind: &str, hash: &str, part: Option<u64>) -> Result<Entity> {
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
            .query_one(&selector)?
            .with_context(|| format!("missing dependency: {selector}"))?;
        let entity = self.entity(&key)?;
        ensure!(
            entity.kind == kind
                && entity.attributes["hash"]["value"] == hash
                && part.is_none_or(|part| entity.attributes["part"]["value"] == part),
            "queried dependency attributes do not match requested locator"
        );
        Ok(entity)
    }
}

/// Restore the processor's original typed values before checking its staging hash.
fn rpc_wire(entity: &Value, requested_key: &str, owner: &str) -> Result<Value> {
    ensure!(
        hex_value(wire::string(entity, "key")?, 32)? == requested_key,
        "RPC entity key mismatch"
    );
    for name in ["owner", "creator"] {
        ensure!(
            hex_value(wire::string(entity, name)?, 20)? == owner,
            "entity {name} outside configured namespace"
        );
    }
    let mut result = Map::new();
    for attribute in entity["attributes"]
        .as_array()
        .context("missing entity attributes")?
    {
        let name = wire::string(attribute, "name")?;
        ensure!(
            !name.starts_with('$')
                && name != "projectId"
                && !name.is_empty()
                && !result.contains_key(name),
            "invalid or duplicate attribute name"
        );
        let kind = wire::string(attribute, "type")?;
        let value = &attribute["value"];
        let restored = match kind {
            "str" => value
                .as_str()
                .context("invalid string attribute")?
                .to_owned(),
            "addr" => checksum_address(value.as_str().context("invalid address attribute")?)?,
            "bytes32" | "key" => {
                hex_value(value.as_str().context("invalid hash/key attribute")?, 32)?
            }
            "u64" => quantity(value)?.to_string(),
            "i32" => {
                let number = value.as_i64().context("invalid i32 attribute")?;
                i32::try_from(number)
                    .context("i32 attribute out of range")?
                    .to_string()
            }
            "bool" => value
                .as_bool()
                .context("invalid bool attribute")?
                .to_string(),
            _ => bail!("unsupported RPC attribute type {kind}"),
        };
        result.insert(name.into(), json!(format!("{kind}:{restored}")));
    }
    let mut result = Value::Object(result);
    ensure!(
        wire::typed(&result, "projectid", "str:")? == "sourcify",
        "entity project outside namespace"
    );
    ensure!(
        wire::typed(&result, "uploadschema", "str:")? == "sourcify-arkiv.upload.v1",
        "unsupported upload schema"
    );
    let staging: [u8; 32] = hex::decode(wire::typed(&result, "stagingid", "bytes32:0x")?)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid staging ID"))?;
    ensure!(
        entity_key(&staging) == requested_key,
        "RPC key does not match prefill staging identity"
    );
    let content_type = wire::string(entity, "contentType")?;
    let original = wire::typed(&result, "originalcontenttype", "str:")?;
    let encoding = wire::typed(&result, "contentencoding", "str:")?;
    ensure!(
        match encoding {
            "gzip" => content_type == "application/gzip",
            "identity" => content_type == original,
            _ => false,
        },
        "invalid entity content type/encoding"
    );
    let payload = wire::string(entity, "payload")?
        .strip_prefix("0x")
        .context("RPC payload must be hex")?;
    ensure!(
        payload.len() <= BUDGET * 2,
        "encoded entity payload exceeds limit"
    );
    result["$payload"] =
        json!(STANDARD.encode(hex::decode(payload).context("invalid RPC payload hex")?));
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hx, sha, transform};
    use flate2::{write::GzEncoder, Compression};
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
        thread,
    };

    const OWNER: &str = "0x986c08292e6c327a310273a45965a3d2e2eac44c";
    const ADDRESS: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";

    fn fixture() -> (Value, Vec<Value>) {
        let record = json!({"chainId":"1", "address":ADDRESS,"matchId":"12345678901234567",
            "verifiedAt":"2020-01-01T00:00:00Z", "abi":[],
            "compilation":{"language":"Solidity","compiler":"solc","compilerVersion":"1",
                "compilerSettings":{},"fullyQualifiedName":"A.sol:A"},
            "sources":{"A.sol":{"content":"contract A {}".repeat(25_000)}},
            "creationBytecode":{"onchainBytecode":null,"recompiledBytecode":null},
            "runtimeBytecode":{"onchainBytecode":"0x6000","recompiledBytecode":"0x6000"},
            "deployment":{"deployer":"0x52908400098527886E0F7030069857D2E4169EE7"}});
        let entities = transform(&record).unwrap().iter().map(rpc_entity).collect();
        (record, entities)
    }

    fn rpc_entity(entity: &Entity) -> Value {
        let mut attrs = Vec::new();
        for (name, attribute) in &entity.attributes {
            let kind = attribute["type"].as_str().unwrap();
            let original = &attribute["value"];
            let value = match kind {
                "key" => json!(entity_key(
                    &hex::decode(original.as_str().unwrap())
                        .unwrap()
                        .try_into()
                        .unwrap()
                )),
                "addr" => json!(original.as_str().unwrap().to_ascii_lowercase()),
                "u64" => json!(format!(
                    "0x{:x}",
                    original.as_str().unwrap().parse::<u64>().unwrap()
                )),
                _ => original.clone(),
            };
            attrs.push(json!({"name":if name=="projectId" {"projectid"} else {name}, "type":kind,"value":value}));
        }
        let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
        gzip.write_all(&entity.payload).unwrap();
        let payload = gzip.finish().unwrap();
        for (name, kind, value) in [
            ("contentencoding", "str", "gzip".into()),
            ("originalcontenttype", "str", entity.content_type.clone()),
            ("payloadsha256", "bytes32", hx(sha(&entity.payload))),
            ("stagingid", "bytes32", hx(entity.id)),
            ("uploadschema", "str", "sourcify-arkiv.upload.v1".into()),
        ] {
            attrs.push(json!({"name":name,"type":kind,"value":value}));
        }
        json!({"key":entity_key(&entity.id),"owner":OWNER,"creator":OWNER,
            "contentType":"application/gzip","payload":hx(payload),"attributes":attrs})
    }

    fn attribute<'a>(entity: &'a Value, name: &str) -> &'a Value {
        &entity["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == name)
            .unwrap()["value"]
    }
    fn change_attribute(entity: &mut Value, name: &str, value: Value) {
        entity["attributes"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|v| v["name"] == name)
            .unwrap()["value"] = value;
    }

    struct Mock {
        url: String,
        requests: Arc<Mutex<Vec<Value>>>,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }
    impl Drop for Mock {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            self.worker.take().unwrap().join().unwrap();
        }
    }
    impl Mock {
        fn new(entities: Vec<Value>, fault: &'static str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let (stop_worker, recorded) = (stop.clone(), requests.clone());
            let worker = thread::spawn(move || {
                let mut block_checks = 0;
                while !stop_worker.load(Ordering::Relaxed) {
                    let (mut socket, _) = match listener.accept() {
                        Ok(value) => value,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(e) => panic!("mock accept: {e}"),
                    };
                    socket
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut reader = BufReader::new(socket.try_clone().unwrap());
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let mut length = 0;
                    loop {
                        line.clear();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" {
                            break;
                        }
                        if let Some((name, value)) = line.split_once(':') {
                            if name.eq_ignore_ascii_case("content-length") {
                                length = value.trim().parse().unwrap();
                            }
                        }
                    }
                    let mut bytes = vec![0; length];
                    reader.read_exact(&mut bytes).unwrap();
                    let request: Value = serde_json::from_slice(&bytes).unwrap();
                    recorded.lock().unwrap().push(request.clone());
                    let params = &request["params"];
                    let result = match request["method"].as_str().unwrap() {
                        "eth_chainId" => json!("0x75992f"),
                        "eth_getBlockByNumber" => {
                            block_checks += 1;
                            json!({"number":params[0],"hash":format!("0x{}", if fault=="reorg" && block_checks>1 {"22"}else{"11"}.repeat(32))})
                        }
                        "arkiv_getEntity" => {
                            assert_eq!(
                                params[1], 42,
                                "getEntity must use the numeric pinned block"
                            );
                            entities
                                .iter()
                                .find(|e| e["key"] == params[0])
                                .cloned()
                                .unwrap_or(Value::Null)
                        }
                        "arkiv_query" => {
                            assert_eq!(params[1]["atBlock"], "0x2a");
                            assert_eq!(params[1]["limit"], "0x2");
                            let query = params[0].as_str().unwrap();
                            assert!(query.contains("projectid = str('sourcify')"));
                            assert!(query.contains(&format!("$owner = addr({OWNER})")));
                            let mut found: Vec<Value> = entities
                                .iter()
                                .filter(|e| {
                                    let kind = attribute(e, "kind").as_str().unwrap();
                                    if !query.contains(&format!("kind = str('{kind}')")) {
                                        return false;
                                    }
                                    if kind == "verified_contract" {
                                        return query
                                            .contains(&format!("address = addr({ADDRESS})"));
                                    }
                                    if !query.contains(&format!(
                                        "hash = bytes32({})",
                                        attribute(e, "hash").as_str().unwrap()
                                    )) {
                                        return false;
                                    }
                                    kind != "blob"
                                        || query.contains(&format!(
                                            "part = i32({})",
                                            attribute(e, "part")
                                        ))
                                })
                                .map(|e| json!({"key":e["key"]}))
                                .collect();
                            if fault == "missing" && !query.contains("verified_contract") {
                                found.clear();
                            }
                            if fault == "ambiguous" && !found.is_empty() {
                                found.push(found[0].clone());
                            }
                            let mut result = json!({"data":found,"blockNumber":if fault=="wrong-block" {"0x2b"}else{"0x2a"}});
                            if fault == "cursor" {
                                result["cursor"] = json!("more");
                            }
                            result
                        }
                        method => panic!("unexpected method {method}"),
                    };
                    let response = if fault == "rpc-error" {
                        json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32603,"message":"unavailable"}})
                    } else {
                        json!({"jsonrpc":"2.0","id":if fault=="wrong-id" {json!(999)}else{request["id"].clone()},"result":result})
                    };
                    if fault == "slow" {
                        thread::sleep(Duration::from_millis(100));
                    }
                    let bytes = serde_json::to_vec(&response).unwrap();
                    let status = if fault == "http-error" {
                        "503 Service Unavailable"
                    } else {
                        "200 OK"
                    };
                    let _=write!(socket,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len());
                    let _ = socket.write_all(&bytes);
                }
            });
            Self {
                url,
                requests,
                stop,
                worker: Some(worker),
            }
        }
        fn config(&self) -> ReadConfig {
            ReadConfig {
                rpc_url: self.url.clone(),
                owner: OWNER.into(),
                arkiv_chain_id: 7706927,
                block: 42,
                timeout: Duration::from_secs(2),
                deadline: Duration::from_secs(10),
                max_rpc_requests: 100,
                max_rpc_response_bytes: 1048576,
            }
        }
    }

    #[test]
    fn rpc_hydrates_full_spilled_contract_and_pins_every_read() {
        let (record, entities) = fixture();
        assert!(entities.iter().any(|e| attribute(e, "kind") == "blob"));
        let mock = Mock::new(entities, "");
        let result = read_contract(mock.config(), ADDRESS).unwrap();
        assert_eq!(result.contract, record);
        assert_eq!(result.block, 42);
        let requests = mock.requests.lock().unwrap();
        let mut seen = std::collections::HashSet::new();
        for request in requests.iter().filter(|r| r["method"] == "arkiv_getEntity") {
            assert!(
                seen.insert(request["params"][0].clone().to_string()),
                "entity refetch escaped cache"
            );
        }
        assert_eq!(result.rpc_requests, requests.len() as u64);
    }

    #[test]
    fn query_ambiguity_cursor_and_wrong_block_fail_closed() {
        for (fault, message) in [
            ("ambiguous", "ambiguous"),
            ("cursor", "ambiguous"),
            ("wrong-block", "different block"),
            ("reorg", "block changed"),
        ] {
            let mock = Mock::new(fixture().1, fault);
            let error = read_contract(mock.config(), ADDRESS).err().unwrap();
            assert!(format!("{error:#}").contains(message), "{fault}: {error:#}");
        }
    }

    #[test]
    fn absent_contract_is_distinct_from_missing_dependency_and_rpc_failure() {
        for (fault, address, message) in [
            (
                "",
                "0x1111111111111111111111111111111111111111",
                "contract not found",
            ),
            ("missing", ADDRESS, "missing dependency"),
            ("rpc-error", ADDRESS, "returned error"),
            ("http-error", ADDRESS, "HTTP 503"),
            ("wrong-id", ADDRESS, "response ID"),
        ] {
            let mock = Mock::new(fixture().1, fault);
            let error = read_contract(mock.config(), address).err().unwrap();
            assert!(format!("{error:#}").contains(message), "{fault}: {error:#}");
        }
    }

    #[test]
    fn namespace_types_hashes_and_payload_corruption_are_rejected() {
        for fault in [
            "owner",
            "creator",
            "project",
            "project-alias",
            "schema",
            "duplicate",
            "type",
            "key",
            "payload",
            "checksum",
            "address",
        ] {
            let mut entities = fixture().1;
            let entity = entities
                .iter_mut()
                .find(|e| attribute(e, "kind") == "verified_contract")
                .unwrap();
            match fault {
                "owner" | "creator" => {
                    entity[fault] = json!("0x1111111111111111111111111111111111111111")
                }
                "project" => change_attribute(entity, "projectid", json!("other")),
                "project-alias" => entity["attributes"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"name":"projectId","type":"str","value":"other"})),
                "schema" => change_attribute(entity, "uploadschema", json!("future")),
                "duplicate" => {
                    let a = entity["attributes"][0].clone();
                    entity["attributes"].as_array_mut().unwrap().push(a);
                }
                "type" => {
                    entity["attributes"][0]["type"] = json!("future");
                }
                "key" => {
                    change_attribute(entity, "stagingid", json!(format!("0x{}", "00".repeat(32))))
                }
                "payload" => entity["payload"] = json!("0xdeadbeef"),
                "checksum" => change_attribute(
                    entity,
                    "payloadsha256",
                    json!(format!("0x{}", "00".repeat(32))),
                ),
                "address" => change_attribute(
                    entity,
                    "address",
                    json!("0x1111111111111111111111111111111111111111"),
                ),
                _ => unreachable!(),
            }
            let mock = Mock::new(entities, "");
            assert!(
                read_contract(mock.config(), ADDRESS).is_err(),
                "accepted {fault}"
            );
        }
    }

    #[test]
    fn rpc_count_deadline_and_envelope_size_are_bounded() {
        let mock = Mock::new(fixture().1, "");
        let mut config = mock.config();
        config.max_rpc_requests = 2;
        assert!(read_contract(config, ADDRESS)
            .err()
            .unwrap()
            .to_string()
            .contains("budget"));
        let mut config = mock.config();
        config.deadline = Duration::from_nanos(1);
        assert!(read_contract(config, ADDRESS)
            .err()
            .unwrap()
            .to_string()
            .contains("deadline"));
        let mut config = mock.config();
        config.max_rpc_response_bytes = 10;
        assert!(read_contract(config, ADDRESS)
            .err()
            .unwrap()
            .to_string()
            .contains("response limit"));
    }

    #[test]
    fn corrupt_blob_payload_is_rejected_through_rpc_resolution() {
        let mut entities = fixture().1;
        let blob = entities
            .iter_mut()
            .find(|e| attribute(e, "kind") == "blob")
            .unwrap();
        blob["payload"] = json!("0xdeadbeef");
        let mock = Mock::new(entities, "");
        let error = read_contract(mock.config(), ADDRESS).err().unwrap();
        assert!(format!("{error:#}").contains("corrupt Arkiv entity"));
        assert!(mock.requests.lock().unwrap().iter().any(|request| {
            request["method"] == "arkiv_query"
                && request["params"][0]
                    .as_str()
                    .unwrap()
                    .contains("part = i32(")
        }));
    }

    #[test]
    fn compressed_entities_retain_existing_decoded_size_limit() {
        let mut entities = fixture().1;
        let entity = entities
            .iter_mut()
            .find(|e| attribute(e, "kind") == "verified_contract")
            .unwrap();
        let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
        gzip.write_all(&vec![b'x'; BUDGET + 1]).unwrap();
        entity["payload"] = json!(hx(gzip.finish().unwrap()));
        let mock = Mock::new(entities, "");
        let error = read_contract(mock.config(), ADDRESS).err().unwrap();
        assert!(format!("{error:#}").contains("decoded wire payload exceeds limit"));
    }

    #[test]
    fn individual_rpc_calls_timeout() {
        let mock = Mock::new(fixture().1, "slow");
        let mut config = mock.config();
        config.timeout = Duration::from_millis(10);
        let error = read_contract(config, ADDRESS).err().unwrap();
        assert!(format!("{error:#}").contains("transport failed"));
    }
}
