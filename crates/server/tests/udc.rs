//! The v4 (UDC) backend over real async HTTP JSON-RPC, against a mock node holding the entities
//! `udc::Writer` builds. Every response body must equal, byte for byte, the v3 path's for the
//! same records (an in-memory v3 backend), so the HTTP contract does not change with the format.
use arkiv_entity_processor::{
    transform,
    udc::{Profile, UdcEntity, Writer},
    Entity,
};
use arkiv_sourcify_server::{
    backend::{Backend, BackendError, ReadFuture, ReadSession},
    backends::udc::{Config, UdcBackend, View},
    Application, Limits,
};
use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{Request, StatusCode},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tower::ServiceExt;

const OWNER: &str = "0x986c08292e6c327a310273a45965a3d2e2eac44c";
const PREFIX: &str = "sourcify.v4";
const SAFE_PROXY: &str = "0x000000000034065b3a94c2118cfe5b4c0067b615";
const STORED: &str = "0xdae07103f1e0b23eb58d50758b9ade175d33e71b";
const MANY_SOURCES: &str = "0x1111111111111111111111111111111111111111";
const GENESIS: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn captured() -> Value {
    serde_json::from_str(include_str!("../../../docs/data/sourcify-response.json")).unwrap()
}
fn stored() -> Value {
    serde_json::from_str(include_str!("fixtures/sourcify-v2/stored-16511691.json")).unwrap()
}
/// SafeProxy at another address with 20 sources: `fields=sources` needs three key batches.
fn many_sources() -> Value {
    let mut record = captured();
    record["address"] = MANY_SOURCES.into();
    record["matchId"] = "3".into();
    for i in 0..20 {
        record["sources"][format!("lib/Part{i}.sol")] =
            json!({"content": format!("// part {i}\ncontract Part{i} {{}}\n")});
    }
    record
}
fn records() -> Vec<Value> {
    vec![captured(), stored(), many_sources()]
}

// --- v3 reference: the same records through the v3 hydration path, in memory ---------------

#[derive(Clone)]
struct Memory(Arc<Vec<Entity>>);
impl Backend for Memory {
    fn begin_read(&self) -> ReadFuture<'_, Box<dyn ReadSession>> {
        Box::pin(async { Ok(Box::new(self.clone()) as Box<dyn ReadSession>) })
    }
    fn metadata(&self) -> Value {
        json!({"backend":"v3-memory"})
    }
}
impl ReadSession for Memory {
    fn find_contract<'a>(
        &'a mut self,
        _: &'a str,
        address: &'a str,
    ) -> ReadFuture<'a, Option<Entity>> {
        Box::pin(async move {
            Ok(self
                .0
                .iter()
                .find(|e| {
                    e.kind == "verified_contract"
                        && e.attributes["address"]["value"]
                            .as_str()
                            .is_some_and(|a| a.eq_ignore_ascii_case(address))
                })
                .cloned())
        })
    }
    fn resolve<'a>(
        &'a mut self,
        kind: &'a str,
        hash: &'a str,
        part: Option<u64>,
    ) -> ReadFuture<'a, Entity> {
        Box::pin(async move {
            self.0
                .iter()
                .find(|e| {
                    e.kind == kind
                        && e.attributes["hash"]["value"] == hash
                        && part.is_none_or(|p| e.attributes["part"]["value"] == p)
                })
                .cloned()
                .ok_or_else(|| BackendError::MissingDependency(format!("{kind} {hash}")).into())
        })
    }
    fn finish(&mut self) -> ReadFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}
fn v3_application() -> Application {
    v3_application_for(&records())
}
fn v3_application_for(records: &[Value]) -> Application {
    let entities = records.iter().flat_map(|r| transform(r).unwrap()).collect();
    Application::new(Arc::new(Memory(Arc::new(entities))), Limits::default()).unwrap()
}

// --- the mock UDC node ------------------------------------------------------------------------

struct MockState {
    entities: Vec<UdcEntity>,
    fault: Mutex<&'static str>,
    requests: Mutex<Vec<Value>>,
    /// Requests still to answer busy: HTTP 429, then `-32010 ServerBusy`, alternately.
    busy: AtomicUsize,
}
struct Mock {
    state: Arc<MockState>,
    url: String,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.server.abort();
    }
}
fn owner() -> [u8; 20] {
    hex::decode(&OWNER[2..]).unwrap().try_into().unwrap()
}
impl Mock {
    async fn new() -> Self {
        let mut writer = Writer::new(
            Profile {
                prefix: PREFIX.into(),
                chain_id: 7706815,
                owner: owner(),
            },
            0,
        );
        let mut entities = vec![];
        for record in records() {
            entities.extend(writer.contract(&transform(&record).unwrap()).unwrap());
        }
        let state = Arc::new(MockState {
            entities,
            fault: Mutex::new("none"),
            requests: Mutex::new(vec![]),
            busy: AtomicUsize::new(0),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/", post(rpc))
            .with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { state, url, server }
    }
    fn config(&self, view: View) -> Config {
        let mut config = Config::new(self.url.clone(), OWNER.into(), 7706815);
        config.view = view;
        config.genesis_hash = Some(GENESIS.into());
        config
    }
    async fn application(&self, view: View) -> Application {
        Application::new(
            Arc::new(UdcBackend::connect(self.config(view)).await.unwrap()),
            Limits::default(),
        )
        .unwrap()
    }
    fn fault(&self, fault: &'static str) {
        *self.state.fault.lock().unwrap() = fault;
    }
    fn queries(&self) -> Vec<Value> {
        self.state
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == "arkiv_query")
            .cloned()
            .collect()
    }
}
/// An entity as `arkiv_query` renders it: `attributeSchema` lists every attribute, `attributes`
/// only the selected ones (without the indexed flag).
fn render(e: &UdcEntity, select: &Value) -> Value {
    let v = e.to_json();
    let all = v["attributes"].as_array().unwrap();
    let keep = |name: &str| match &select["attributes"] {
        Value::Bool(b) => *b,
        Value::Object(names) => names.get(name) == Some(&json!(true)),
        _ => false,
    };
    let mut out = json!({"key": v["key"]});
    for field in ["owner", "creator", "expiresAt"] {
        if select[field] == true {
            out[field] = v[field].clone();
        }
    }
    if select["attributeSchema"] == true {
        out["attributeSchema"] = all
            .iter()
            .map(|a| json!({"name":a["name"],"type":a["type"],"indexed":a["indexed"]}))
            .collect();
    }
    out["attributes"] = all
        .iter()
        .filter(|a| keep(a["name"].as_str().unwrap()))
        .map(|a| json!({"name":a["name"],"type":a["type"],"value":a["value"]}))
        .collect();
    out
}
fn hex_terms<'a>(query: &'a str, marker: &str) -> Vec<&'a str> {
    query
        .match_indices(marker)
        .map(|(i, _)| {
            let rest = &query[i + marker.len()..];
            &rest[..rest.find(')').unwrap()]
        })
        .collect()
}
async fn rpc(
    State(state): State<Arc<MockState>>,
    Json(request): Json<Value>,
) -> (StatusCode, Json<Value>) {
    state.requests.lock().unwrap().push(request.clone());
    let ok = |result: Value| {
        (
            StatusCode::OK,
            Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result})),
        )
    };
    let error = |code: i64, message: &str| {
        (
            StatusCode::OK,
            Json(
                json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":code,"message":message}}),
            ),
        )
    };
    let busy = state
        .busy
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
    if let Ok(left) = busy {
        return match left % 2 {
            1 => (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error":"busy"}))),
            _ => error(-32010, "ServerBusy"),
        };
    }
    let fault = *state.fault.lock().unwrap();
    let params = &request["params"];
    let head = 0x2a;
    match request["method"].as_str().unwrap() {
        "eth_chainId" => ok(json!("0x7598bf")),
        "eth_blockNumber" => ok(json!(format!("0x{head:x}"))),
        "eth_getBlockByNumber" => {
            let selector = params[0].as_str().unwrap();
            let number = match selector {
                "latest" => format!("0x{head:x}"),
                other => other.to_owned(),
            };
            let hash = match (selector, number.as_str(), fault) {
                (_, "0x0", _) => GENESIS.to_owned(),
                ("0x2a", _, "reorg") => format!("0x{}", "22".repeat(32)),
                _ => format!("0x{}", "33".repeat(32)),
            };
            ok(json!({"number": number, "hash": hash}))
        }
        "arkiv_query" => {
            let query = params[0].as_str().unwrap();
            let options = &params[1];
            if let Some(at) = options["atBlock"].as_str() {
                if fault == "head-only" && at != format!("0x{head:x}") {
                    return error(
                        -32006,
                        "block is unavailable: state for this block is not retained",
                    );
                }
            }
            let block = options["atBlock"]
                .as_str()
                .map(String::from)
                .unwrap_or(format!("0x{head:x}"));
            let limit = usize::from_str_radix(
                options["limit"].as_str().unwrap().trim_start_matches("0x"),
                16,
            )
            .unwrap();
            let mut found: Vec<&UdcEntity> = if query.contains("arkiv.schema = str('") {
                let tag = format!("{PREFIX}.contract");
                assert!(
                    query.contains(&format!("arkiv.schema = str('{tag}')")),
                    "{query}"
                );
                assert!(
                    query.contains(&format!("$owner = addr({OWNER})")),
                    "{query}"
                );
                assert!(query.contains("chainid = u64(1)"), "{query}");
                let address = hex_terms(query, "address = addr(")[0].to_owned();
                state
                    .entities
                    .iter()
                    .filter(|e| {
                        e.schema() == Some(tag.as_str())
                            && e.to_json()["attributes"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|a| a["name"] == "address" && a["value"] == address.as_str())
                    })
                    .collect()
            } else {
                let keys = hex_terms(query, "$key = key(");
                assert!(!keys.is_empty() && !query.contains(" AND "), "{query}");
                state
                    .entities
                    .iter()
                    .filter(|e| keys.contains(&format!("0x{}", hex::encode(e.key)).as_str()))
                    .filter(|e| {
                        fault != "missing" || !e.schema().is_some_and(|s| s.ends_with(".source"))
                    })
                    .collect()
            };
            if fault == "ambiguous" && found.len() == 1 && query.contains("address") {
                found.push(found[0]);
            }
            let cursor = found.len() > limit;
            found.truncate(limit);
            let mut rows: Vec<Value> = found
                .iter()
                .map(|e| render(e, &options["select"]))
                .collect();
            if fault == "owner" {
                for row in &mut rows {
                    if row["attributes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|a| a["value"] == format!("{PREFIX}.compilation"))
                    {
                        row["owner"] = json!(format!("0x{}", "99".repeat(20)));
                        row["creator"] = row["owner"].clone();
                    }
                }
            }
            let mut result = json!({"data": rows, "blockNumber": if fault == "wrong-block" {"0x2b".into()} else {block}});
            if cursor {
                result["cursor"] = json!("more");
            }
            ok(result)
        }
        method => panic!("unexpected method {method}"),
    }
}

async fn get(router: Router, path: &str) -> (StatusCode, Vec<u8>) {
    let response = router
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
}
/// Error IDs are fresh UUIDs per response; everything else must be identical.
fn normalized(body: &[u8]) -> String {
    let text = String::from_utf8(body.to_vec()).unwrap();
    let mut out = String::new();
    let mut rest = text.as_str();
    while let Some(i) = rest.find("\"errorId\":\"") {
        let start = i + "\"errorId\":\"".len();
        out.push_str(&rest[..start]);
        out.push_str("<uuid>");
        rest = &rest[start + 36..];
    }
    out.push_str(rest);
    out
}
fn json_body(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap()
}

const QUERIES: &[&str] = &[
    "",
    "?fields=all",
    "?fields=abi,compilation",
    "?omit=proxyResolution",
    "?fields=sources",
    "?fields=metadata",
    "?fields=signatures",
    "?fields=abi,signatures",
    "?fields=stdJsonInput,stdJsonOutput",
    "?fields=creationBytecode,runtimeBytecode",
    "?fields=creationBytecode.sourceMap,runtimeBytecode.onchainBytecode,runtimeBytecode.immutableReferences",
    "?fields=deployment",
    "?fields=compilation.name",
    "?fields=proxyResolution",
    "?fields=proxyResolution.onchainRuntimeBytecode,proxyResolution.onchainCreationBytecode",
    "?fields=storageLayout,transientStorageLayout,userdoc,devdoc,sourceIds,additionalInput",
    "?fields=compilation.language,deployment.transactionHash,runtimeBytecode.immutableReferences,storageLayout,transientStorageLayout",
    "?omit=creationBytecode,runtimeBytecode,sources,compilation,abi,metadata,storageLayout,transientStorageLayout,userdoc,devdoc,sourceIds,additionalInput,stdJsonInput,stdJsonOutput,signatures,proxyResolution",
    "?omit=creationBytecode,runtimeBytecode,sources,compilation,abi,metadata,storageLayout,transientStorageLayout,userdoc,devdoc,sourceIds,additionalInput,stdJsonInput,stdJsonOutput,signatures,proxyResolution,deployment.transactionHash,deployment.blockNumber",
    "?omit=creationBytecode,runtimeBytecode,sources,compilation,abi,metadata,storageLayout,transientStorageLayout,userdoc,devdoc,sourceIds,additionalInput,stdJsonInput,stdJsonOutput,signatures,proxyResolution,matchId,creationMatch,runtimeMatch,verifiedAt",
    "?fields=nope",
];

#[tokio::test]
async fn every_selection_answers_byte_for_byte_like_the_v3_path() {
    let mock = Mock::new().await;
    let v4 = mock.application(View::Pinned).await;
    let v3 = v3_application();
    let mut compared = 0;
    for address in [
        SAFE_PROXY,
        STORED,
        MANY_SOURCES,
        "0x2222222222222222222222222222222222222222",
    ] {
        for query in QUERIES {
            let path = format!("/v2/contract/1/{address}{query}");
            let (status3, body3) = get(v3.router(), &path).await;
            let (status4, body4) = get(v4.router(), &path).await;
            assert_eq!(
                status4,
                status3,
                "{path}: {}",
                String::from_utf8_lossy(&body4)
            );
            assert_eq!(normalized(&body4), normalized(&body3), "{path}");
            compared += 1;
        }
    }
    assert_eq!(compared, 4 * QUERIES.len());
    // The full record equals the captured response, as the v3 tests assert.
    let (status, body) = get(
        v4.router(),
        &format!("/v2/contract/1/{SAFE_PROXY}?omit=proxyResolution"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let selection = arkiv_sourcify_server::fields::parse(Some("omit=proxyResolution")).unwrap();
    assert_eq!(json_body(&body), selection.project(captured()).unwrap());
    // Every key query names at most 8 keys, and the 20-source read used full batches of 8.
    let queries = mock.queries();
    let mut widest = 0;
    for q in &queries {
        let text = q["params"][0].as_str().unwrap();
        let keys = text.matches("$key = key(").count();
        assert!(keys <= 8, "{text}");
        widest = widest.max(keys);
        assert_eq!(q["params"][1]["atBlock"], "0x2a");
    }
    assert_eq!(widest, 8);
}

#[tokio::test]
async fn the_hot_read_takes_two_queries_and_selects_only_what_it_uses() {
    let mock = Mock::new().await;
    let app = mock.application(View::Pinned).await;
    mock.state.requests.lock().unwrap().clear();
    let (status, body) = get(
        app.router(),
        &format!("/v2/contract/1/{SAFE_PROXY}?fields=abi,compilation"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let queries = mock.queries();
    assert_eq!(queries.len(), 2, "{queries:#?}");
    assert_eq!(queries[0]["params"][1]["select"]["attributes"], true);
    let mut names: Vec<&str> = queries[1]["params"][1]["select"]["attributes"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["abi", "arkiv.schema", "compilation", "hash", "manifest"]
    );
    assert_eq!(
        queries[1]["params"][0]
            .as_str()
            .unwrap()
            .matches("$key")
            .count(),
        1
    );
    // A full read: the contract, then its compilation and code, then sources and recompiled code.
    mock.state.requests.lock().unwrap().clear();
    let (status, _) = get(
        app.router(),
        &format!("/v2/contract/1/{SAFE_PROXY}?omit=proxyResolution"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(mock.queries().len(), 3);
}

#[tokio::test]
async fn pinned_reads_pin_every_query_and_a_changed_block_fails() {
    let mock = Mock::new().await;
    let app = mock.application(View::Pinned).await;
    mock.state.requests.lock().unwrap().clear();
    assert_eq!(
        get(app.router(), &format!("/v2/contract/1/{SAFE_PROXY}"))
            .await
            .0,
        StatusCode::OK
    );
    let requests = mock.state.requests.lock().unwrap().clone();
    assert!(requests
        .iter()
        .filter(|r| r["method"] == "arkiv_query")
        .all(|r| r["params"][1]["atBlock"] == "0x2a"));
    assert_eq!(requests.last().unwrap()["method"], "eth_getBlockByNumber");
    assert_eq!(requests.last().unwrap()["params"][0], "0x2a");
    for (fault, code) in [("reorg", "state_changed"), ("wrong-block", "state_changed")] {
        mock.fault(fault);
        let (status, body) = get(app.router(), &format!("/v2/contract/1/{SAFE_PROXY}")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{fault}");
        assert_eq!(json_body(&body)["customCode"], code, "{fault}");
    }
}

#[tokio::test]
async fn head_reads_send_no_block_and_auto_detects_a_head_only_node() {
    let mock = Mock::new().await;
    let app = mock.application(View::Head).await;
    mock.state.requests.lock().unwrap().clear();
    let (status, body) = get(
        app.router(),
        &format!("/v2/contract/1/{SAFE_PROXY}?omit=proxyResolution"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let selection = arkiv_sourcify_server::fields::parse(Some("omit=proxyResolution")).unwrap();
    assert_eq!(json_body(&body), selection.project(captured()).unwrap());
    assert!(mock
        .queries()
        .iter()
        .all(|q| q["params"][1].get("atBlock").is_none()));
    // auto: an archive node is read pinned, a head-only node at its head.
    let pinned = UdcBackend::connect(mock.config(View::Auto)).await.unwrap();
    assert_eq!(pinned.read_view(), "pinned");
    mock.fault("head-only");
    let head = UdcBackend::connect(mock.config(View::Auto)).await.unwrap();
    assert_eq!(head.read_view(), "head");
    assert_eq!(head.metadata()["format"], "udc");
    assert_eq!(head.metadata()["schema"], "sourcify.v4.*");
    let app = Application::new(Arc::new(head), Limits::default()).unwrap();
    mock.state.requests.lock().unwrap().clear();
    assert_eq!(
        get(app.router(), &format!("/v2/contract/1/{SAFE_PROXY}"))
            .await
            .0,
        StatusCode::OK
    );
    assert!(mock
        .queries()
        .iter()
        .all(|q| q["params"][1].get("atBlock").is_none()));
    let (status, body) = get(app.router(), "/_ops/ready").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_body(&body)["dataset"]["readView"], "head (no history)");
}

#[tokio::test]
async fn missing_foreign_and_ambiguous_entities_fail_closed() {
    let mock = Mock::new().await;
    let app = mock.application(View::Pinned).await;
    for (fault, status, code) in [
        ("missing", StatusCode::BAD_GATEWAY, "missing_dependency"),
        ("owner", StatusCode::BAD_GATEWAY, "corrupt_data"),
        ("ambiguous", StatusCode::BAD_GATEWAY, "ambiguous_record"),
    ] {
        mock.fault(fault);
        let (actual, body) = get(
            app.router(),
            &format!("/v2/contract/1/{SAFE_PROXY}?fields=all"),
        )
        .await;
        assert_eq!(
            actual,
            status,
            "{fault}: {}",
            String::from_utf8_lossy(&body)
        );
        let body = json_body(&body);
        assert!(body.get("address").is_none(), "{fault}");
        assert_eq!(body["customCode"], code, "{fault}");
    }
    // A wrong genesis or network never connects.
    mock.fault("none");
    let mut config = mock.config(View::Pinned);
    config.genesis_hash = Some(format!("0x{}", "bb".repeat(32)));
    assert!(UdcBackend::connect(config).await.is_err());
    let mut config = mock.config(View::Pinned);
    config.arkiv_chain_id = 1;
    assert!(UdcBackend::connect(config).await.is_err());
}

#[tokio::test]
async fn debug_traces_the_v4_read_and_work_limits_hold() {
    let mock = Mock::new().await;
    let app = mock.application(View::Pinned).await;
    mock.state.requests.lock().unwrap().clear();
    let (status, body) = get(
        app.router(),
        &format!("/v2/debug/contract/1/{SAFE_PROXY}?fields=abi"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = json_body(&body);
    let calls = body["trace"]["rpcCalls"].as_array().unwrap();
    let requests = mock.state.requests.lock().unwrap().clone();
    assert_eq!(calls.len(), requests.len());
    for (call, request) in calls.iter().zip(&requests) {
        assert_eq!(call["method"], request["method"]);
        assert_eq!(call["params"], request["params"]);
    }
    // Six keys are read for a full SafeProxy record: a limit of 3 dependency reads stops it.
    let app = Application::new(
        Arc::new(
            UdcBackend::connect(mock.config(View::Pinned))
                .await
                .unwrap(),
        ),
        Limits {
            max_dependency_reads: 3,
            ..Limits::default()
        },
    )
    .unwrap();
    let (status, body) = get(
        app.router(),
        &format!("/v2/contract/1/{SAFE_PROXY}?omit=proxyResolution"),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json_body(&body)["customCode"], "work_limit");
}

#[tokio::test]
async fn a_busy_node_is_asked_again_with_backoff_then_the_read_fails_closed() {
    let mock = Mock::new().await;
    let app = mock.application(View::Pinned).await;
    // Four busy answers (two 429s, two -32010s) are absorbed by the retries.
    mock.state.requests.lock().unwrap().clear();
    mock.state.busy.store(4, Ordering::Relaxed);
    let started = std::time::Instant::now();
    let (status, body) = get(
        app.router(),
        &format!("/v2/contract/1/{SAFE_PROXY}?omit=proxyResolution"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let selection = arkiv_sourcify_server::fields::parse(Some("omit=proxyResolution")).unwrap();
    assert_eq!(json_body(&body), selection.project(captured()).unwrap());
    assert!(started.elapsed() >= std::time::Duration::from_millis(100 + 200 + 400 + 800));
    // A node that stays busy: one call is tried six times, then the read fails as unavailable.
    mock.state.requests.lock().unwrap().clear();
    mock.state.busy.store(1_000, Ordering::Relaxed);
    let (status, body) = get(app.router(), &format!("/v2/contract/1/{SAFE_PROXY}")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json_body(&body)["customCode"], "backend_unavailable");
    assert_eq!(mock.state.requests.lock().unwrap().len(), 6);
    mock.state.busy.store(0, Ordering::Relaxed);
}

/// A live server against our node answers like the v3 path for the same records, for every
/// query. The records come from `UDC_LIVE_RECORDS` (NDJSON, the hydrated records the node's
/// contracts were written from), else the captured SafeProxy:
///
///   UDC_LIVE_SERVER=http://127.0.0.1:3100 [UDC_LIVE_RECORDS=records.ndjson] [UDC_LIVE_OUT=dir] \
///       cargo test --release -p arkiv-sourcify-server --test udc live_server -- --ignored
#[tokio::test]
#[ignore]
async fn live_server_answers_like_the_v3_path() {
    let server = std::env::var("UDC_LIVE_SERVER").expect("UDC_LIVE_SERVER");
    let out = std::env::var("UDC_LIVE_OUT").ok();
    let records: Vec<Value> = match std::env::var("UDC_LIVE_RECORDS") {
        Ok(path) => std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect(),
        Err(_) => vec![captured()],
    };
    let v3 = v3_application_for(&records);
    let client = reqwest::Client::new();
    let mut summary = vec![];
    for record in &records {
        let address = record["address"].as_str().unwrap().to_ascii_lowercase();
        for query in QUERIES {
            let path = format!("/v2/contract/1/{address}{query}");
            let (status3, body3) = get(v3.router(), &path).await;
            let response = client.get(format!("{server}{path}")).send().await.unwrap();
            let status4 = response.status().as_u16();
            let body4 = response.bytes().await.unwrap().to_vec();
            let same = status4 == status3.as_u16() && normalized(&body4) == normalized(&body3);
            summary.push(json!({"address": address, "query": query, "status": status4, "bytes": body4.len(), "identical": same}));
            if let Some(dir) = &out {
                let name = if query.is_empty() {
                    "minimal".to_string()
                } else {
                    query[1..].replace(['=', ',', '.'], "_")
                };
                let name: String = name.chars().take(80).collect();
                let dir = format!("{dir}/{address}");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(format!("{dir}/{name}.v3.json"), &body3).unwrap();
                std::fs::write(format!("{dir}/{name}.v4-live.json"), &body4).unwrap();
            }
        }
    }
    if let Some(dir) = &out {
        std::fs::write(
            format!("{dir}/summary.json"),
            serde_json::to_vec_pretty(&summary).unwrap(),
        )
        .unwrap();
    }
    for s in &summary {
        assert_eq!(s["identical"], true, "{s}");
    }
}
