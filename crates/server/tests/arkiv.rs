//! Protocol tests exercise real async HTTP JSON-RPC, without a bundle adapter.
use arkiv_entity_processor::{entity_key, hx, sha, transform, Entity};
use arkiv_sourcify_server::{
    backends::arkiv::{ArkivBackend, Config},
    ethereum::{Config as EthereumConfig, EthereumRpc},
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
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tower::ServiceExt;

const OWNER: &str = "0x986c08292e6c327a310273a45965a3d2e2eac44c";
const ROOT: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";
const IMPLEMENTATION: &str = "0x41675C099F32341bf84BFc5382aF534df5C7461a";
const ADDRESS: &str = "0x000000000034065b3a94c2118cfe5b4c0067b615";
fn captured() -> Value {
    serde_json::from_str(include_str!("../../../docs/data/sourcify-response.json")).unwrap()
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
        attrs.push(json!({"name":if name=="projectId" {"projectid"} else {name},"type":kind,"value":value}));
    }
    for (name, kind, value) in [
        ("contentencoding", "str", "identity".into()),
        ("originalcontenttype", "str", entity.content_type.clone()),
        ("payloadsha256", "bytes32", hx(sha(&entity.payload))),
        ("stagingid", "bytes32", hx(entity.id)),
        ("uploadschema", "str", "sourcify-arkiv.upload.v1".into()),
    ] {
        attrs.push(json!({"name":name,"type":kind,"value":value}));
    }
    json!({"key":entity_key(&entity.id),"owner":OWNER,"creator":OWNER,"contentType":entity.content_type,"payload":hx(&entity.payload),"attributes":attrs})
}
fn attribute<'a>(entity: &'a Value, name: &str) -> &'a Value {
    &entity["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == name)
        .unwrap()["value"]
}
struct MockState {
    entities: Vec<Value>,
    fault: Mutex<&'static str>,
    requests: Mutex<Vec<Value>>,
    ethereum_requests: Mutex<Vec<Value>>,
    ethereum_fault: Mutex<bool>,
    started: tokio::sync::Notify,
    active: AtomicUsize,
    peak: AtomicUsize,
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
impl Mock {
    async fn new() -> Self {
        Self::with_record(captured()).await
    }
    async fn with_record(record: Value) -> Self {
        Self::with_records(vec![record]).await
    }
    async fn with_records(records: Vec<Value>) -> Self {
        let state = Arc::new(MockState {
            entities: {
                let mut entities: Vec<_> = records
                    .iter()
                    .flat_map(|record| transform(record).unwrap())
                    .map(|entity| rpc_entity(&entity))
                    .collect();
                entities.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
                entities.dedup_by(|a, b| a["key"] == b["key"]);
                entities
            },
            fault: Mutex::new("none"),
            requests: Mutex::new(Vec::new()),
            ethereum_requests: Mutex::new(Vec::new()),
            ethereum_fault: Mutex::new(false),
            started: tokio::sync::Notify::new(),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/", post(rpc))
            .route("/ethereum", post(ethereum_rpc))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { state, url, server }
    }
    fn config(&self) -> Config {
        Config::new(self.url.clone(), OWNER.into(), 7706815, ROOT.into())
    }
    async fn application(&self, limits: Limits) -> Application {
        Application::new(
            Arc::new(ArkivBackend::connect(self.config()).await.unwrap()),
            limits,
        )
        .unwrap()
    }
    async fn with_ethereum(&self) -> Application {
        Application::with_ethereum(
            Arc::new(ArkivBackend::connect(self.config()).await.unwrap()),
            Limits::default(),
            Some(
                EthereumRpc::new(EthereumConfig::new(format!(
                    "{}/ethereum?key=private-endpoint-secret",
                    self.url
                )))
                .unwrap(),
            ),
        )
        .unwrap()
    }
    fn fault(&self, fault: &'static str) {
        *self.state.fault.lock().unwrap() = fault;
    }
}
struct Active(Arc<MockState>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}
async fn rpc(
    State(state): State<Arc<MockState>>,
    Json(request): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let active = state.active.fetch_add(1, Ordering::Relaxed) + 1;
    state.peak.fetch_max(active, Ordering::Relaxed);
    let _active = Active(state.clone());
    state.requests.lock().unwrap().push(request.clone());
    let fault = *state.fault.lock().unwrap();
    if fault == "slow-rpc" {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let method = request["method"].as_str().unwrap();
    let params = &request["params"];
    let result = match method {
        "eth_chainId" => json!(if fault == "network" {
            "0x1"
        } else {
            "0x7598bf"
        }),
        "eth_getBlockByNumber" => {
            let selector = params[0].as_str().unwrap();
            if selector == "0x2a" && fault == "slow-finish" {
                state.started.notify_one();
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            json!({"number":if selector=="0x0" {"0x0"} else {"0x2a"},"hash":if selector=="0x2a" && fault=="reorg" {format!("0x{}","2".repeat(64))} else {format!("0x{}","3".repeat(64))},"stateRoot":if fault=="root" {format!("0x{}","4".repeat(64))} else {ROOT.into()}})
        }
        "arkiv_getEntity" => {
            assert_eq!(
                params[1], 42,
                "getEntity historical selector must be numeric"
            );
            let mut value = state
                .entities
                .iter()
                .find(|e| e["key"] == params[0])
                .cloned()
                .unwrap_or(Value::Null);
            match fault {
                "owner" => value["owner"] = json!(format!("0x{}", "9".repeat(40))),
                "corrupt" => value["payload"] = json!("0x00"),
                "missing-entity" => value = Value::Null,
                _ => {}
            };
            value
        }
        "arkiv_query" => {
            assert_eq!(params[1]["atBlock"], "0x2a");
            assert_eq!(params[1]["limit"], "0x2");
            let query = params[0].as_str().unwrap();
            assert!(query.contains("projectid = str('sourcify')"));
            assert!(query.contains(&format!("$owner = addr({OWNER})")));
            if fault == "slow-query"
                || (fault == "slow-dependency" && !query.contains("verified_contract"))
            {
                state.started.notify_one();
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            let mut found = state
                .entities
                .iter()
                .filter(|e| {
                    let kind = attribute(e, "kind").as_str().unwrap();
                    if !query.contains(&format!("kind = str('{kind}')")) {
                        return false;
                    }
                    if kind == "verified_contract" {
                        return fault != "deleted"
                            && query.contains(&format!(
                                "address = addr({})",
                                attribute(e, "address")
                                    .as_str()
                                    .unwrap()
                                    .to_ascii_lowercase()
                            ));
                    }
                    fault != "missing"
                        && query.contains(&format!(
                            "hash = bytes32({})",
                            attribute(e, "hash").as_str().unwrap()
                        ))
                        && (kind != "blob"
                            || query.contains(&format!("part = i32({})", attribute(e, "part"))))
                })
                .map(|e| json!({"key":e["key"]}))
                .collect::<Vec<_>>();
            if fault == "ambiguous" && !found.is_empty() {
                found.push(found[0].clone());
            }
            let mut result =
                json!({"data":found,"blockNumber":if fault=="wrong-block" {"0x2b"} else {"0x2a"}});
            if fault == "cursor" {
                result["cursor"] = json!("continued");
            }
            result
        }
        _ => panic!("unexpected method {method}"),
    };
    let response = if fault == "rpc-error" {
        json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32603,"message":"mock failure"}})
    } else {
        json!({"jsonrpc":"2.0","id":if fault=="wrong-id" {json!(0)} else {request["id"].clone()},"result":result})
    };
    (
        if fault == "http-error" {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::OK
        },
        Json(response),
    )
}
async fn ethereum_rpc(
    State(state): State<Arc<MockState>>,
    Json(request): Json<Value>,
) -> Json<Value> {
    state
        .ethereum_requests
        .lock()
        .unwrap()
        .push(request.clone());
    if *state.ethereum_fault.lock().unwrap() {
        return Json(
            json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":"private-endpoint-secret"}}),
        );
    }
    let result = match request["method"].as_str().unwrap() {
        "eth_chainId" => json!("0x1"),
        "eth_getBlockByNumber" => json!({"number":"0x99","hash":format!("0x{}", "55".repeat(32))}),
        "eth_getStorageAt" => json!(format!(
            "0x{}{}",
            "00".repeat(12),
            IMPLEMENTATION[2..].to_ascii_lowercase()
        )),
        method => panic!("unexpected Ethereum method: {method}"),
    };
    Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
}

async fn get(router: Router, path: &str) -> (StatusCode, Value) {
    let response = router
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap(),
    )
}
fn uri(fields: &str) -> String {
    format!("/v2/contract/1/{ADDRESS}{fields}")
}

#[tokio::test]
async fn real_http_rpc_hydrates_selected_records_with_one_pinned_view() {
    let mock = Mock::new().await;
    let app = mock.application(Limits::default()).await;
    for query in ["", "?fields=abi,compilation", "?omit=proxyResolution"] {
        let (status, actual) = get(app.router(), &uri(query)).await;
        assert_eq!(status, StatusCode::OK, "{actual}");
        let selection = arkiv_sourcify_server::fields::parse(query.strip_prefix('?')).unwrap();
        assert_eq!(actual, selection.project(captured()).unwrap());
    }
    let requests = mock.state.requests.lock().unwrap();
    assert!(requests.iter().any(|r| r["method"] == "arkiv_query"
        && r["params"][0].as_str().unwrap().contains("compilation")));
}
#[tokio::test]
async fn corrupted_missing_ambiguous_and_changed_views_never_succeed_or_become_absence() {
    let mock = Mock::new().await;
    let app = mock.application(Limits::default()).await;
    for fault in [
        "owner",
        "corrupt",
        "missing-entity",
        "missing",
        "ambiguous",
        "cursor",
        "wrong-block",
        "reorg",
        "wrong-id",
        "rpc-error",
        "http-error",
        "network",
        "root",
    ] {
        mock.fault(fault);
        let (status, body) = get(app.router(), &uri("?omit=proxyResolution")).await;
        assert!(status.is_server_error(), "{fault}: {status} {body}");
        assert!(body.get("address").is_none(), "partial body for {fault}");
    }
}
#[tokio::test]
async fn next_request_observes_deletion_and_checks_consistency_even_for_a_miss() {
    let mock = Mock::new().await;
    let app = mock.application(Limits::default()).await;
    assert_eq!(get(app.router(), &uri("")).await.0, StatusCode::OK);
    mock.fault("deleted");
    assert_eq!(get(app.router(), &uri("")).await.0, StatusCode::NOT_FOUND);
    let requests = mock.state.requests.lock().unwrap();
    assert_eq!(requests.last().unwrap()["method"], "eth_getBlockByNumber");
    assert_eq!(requests.last().unwrap()["params"][0], "0x2a");
}
#[tokio::test]
async fn cancellation_drops_inflight_rpc_and_finalization_keeps_request_admission() {
    let mock = Mock::new().await;
    let app = mock
        .application(Limits {
            max_concurrent_requests: 1,
            ..Limits::default()
        })
        .await;
    for fault in ["slow-query", "slow-dependency", "slow-finish"] {
        mock.fault(fault);
        let router = app.router();
        let task = tokio::spawn(async move { get(router, &uri("?omit=proxyResolution")).await });
        mock.state.started.notified().await;
        assert_eq!(
            get(app.router(), &uri("")).await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            get(app.router(), "/_ops/metrics").await.1["activeWorkers"],
            1
        );
        let issued = mock.state.requests.lock().unwrap().len();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            get(app.router(), "/_ops/metrics").await.1["activeWorkers"],
            0
        );
        assert_eq!(
            mock.state.requests.lock().unwrap().len(),
            issued,
            "cancelled read issued more RPCs"
        );
        mock.fault("none");
        assert_eq!(get(app.router(), &uri("")).await.0, StatusCode::OK);
    }
}
#[tokio::test]
async fn rpc_work_and_body_budgets_fail_closed_and_network_is_checked_before_startup() {
    let mock = Mock::new().await;
    let mut config = mock.config();
    config.arkiv_chain_id = 1;
    assert!(ArkivBackend::connect(config).await.is_err());
    let mut config = mock.config();
    config.max_rpc_response_bytes = 10;
    assert!(ArkivBackend::connect(config).await.is_err());
    let mut config = mock.config();
    config.max_rpc_requests = 3;
    let app = Application::new(
        Arc::new(ArkivBackend::connect(config).await.unwrap()),
        Limits::default(),
    )
    .unwrap();
    assert!(get(app.router(), &uri("?omit=proxyResolution"))
        .await
        .0
        .is_server_error());
    mock.state.requests.lock().unwrap().clear();
    let (status, debug) = get(app.router(), &debug_uri("?omit=proxyResolution")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(debug["response"]["customCode"], "work_limit");
    assert_trace_matches_rpc(&mock, &debug);
    assert_eq!(debug["trace"]["rpcCalls"].as_array().unwrap().len(), 3);
    assert!(
        !debug["trace"]["rpcCalls"]
            .as_array()
            .unwrap()
            .iter()
            .any(|call| call["method"] == "arkiv_query"),
        "budget rejection must not invent an executed query"
    );
}

#[tokio::test]
async fn source_spills_reassemble_and_minimal_fields_do_not_read_blob_payloads() {
    let mut record = captured();
    record["sources"] =
        json!({"large/~source.sol":{"content":"contract Source {}\n".repeat(30_000)}});
    let mock = Mock::with_record(record.clone()).await;
    let app = mock.application(Limits::default()).await;
    assert_eq!(get(app.router(), &uri("")).await.0, StatusCode::OK);
    assert!(!mock
        .state
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|r| r["method"] == "arkiv_query"
            && r["params"][0]
                .as_str()
                .unwrap()
                .contains("kind = str('blob')")));
    let (status, actual) = get(app.router(), &uri("?fields=sources")).await;
    assert_eq!(status, StatusCode::OK, "{actual}");
    assert_eq!(actual["sources"], record["sources"]);
    assert!(mock
        .state
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|r| r["method"] == "arkiv_query"
            && r["params"][0]
                .as_str()
                .unwrap()
                .contains("kind = str('blob')")));
    mock.state.requests.lock().unwrap().clear();
    let (status, debug) = get(app.router(), &debug_uri("?fields=sources")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(debug["response"], actual);
    assert_trace_matches_rpc(&mock, &debug);
    assert!(debug["trace"]["rpcCalls"]
        .as_array()
        .unwrap()
        .iter()
        .any(|call| call["method"] == "arkiv_query"
            && call["params"][0]
                .as_str()
                .unwrap()
                .contains("kind = str('blob')")));
}

#[tokio::test]
async fn global_rpc_gate_bounds_requests_across_concurrent_sessions() {
    let mock = Mock::new().await;
    let mut config = mock.config();
    config.max_rpc_concurrency = 2;
    let app = Application::new(
        Arc::new(ArkivBackend::connect(config).await.unwrap()),
        Limits::default(),
    )
    .unwrap();
    mock.fault("slow-rpc");
    let calls = (0..6).map(|_| {
        let router = app.router();
        async move { get(router, &uri("?omit=proxyResolution")).await }
    });
    for (status, body) in futures_util::future::join_all(calls).await {
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    assert_eq!(mock.state.peak.load(Ordering::Relaxed), 2);
}

fn debug_uri(query: &str) -> String {
    format!("/v2/debug/contract/1/{ADDRESS}{query}")
}

fn assert_trace_matches_rpc(mock: &Mock, body: &Value) {
    let calls = body["trace"]["rpcCalls"].as_array().unwrap();
    assert_eq!(body["trace"]["truncated"], false);
    assert_eq!(body["trace"]["omittedRpcCalls"], 0);
    let mut actual = Vec::new();
    for (network, requests) in [
        ("arkiv", &mock.state.requests),
        ("ethereum", &mock.state.ethereum_requests),
    ] {
        // Socket arrival order can differ from dispatch order for parallel
        // entity reads; monotonic transport IDs recover dispatch order.
        let mut requests = requests.lock().unwrap().clone();
        requests.sort_by_key(|request| request["id"].as_u64().unwrap());
        let traced: Vec<_> = calls
            .iter()
            .filter(|call| call["network"] == network)
            .collect();
        assert_eq!(requests.len(), traced.len());
        for (request, call) in requests.iter().zip(traced) {
            assert_eq!(call["method"], request["method"]);
            assert_eq!(call["params"], request["params"]);
        }
        actual.extend(requests);
    }
    assert_eq!(
        calls.len(),
        actual.len(),
        "debug must perform exactly one read"
    );
    for (index, call) in calls.iter().enumerate() {
        assert_eq!(call["index"], index);
        assert!(call["durationMs"].as_u64().is_some());
        assert!(matches!(
            call["outcome"].as_str(),
            Some("success" | "error" | "cancelled")
        ));
        assert_eq!(
            call.as_object().unwrap().len(),
            6,
            "trace includes only the documented public fields"
        );
    }
    assert!(!body.to_string().contains("private-endpoint-secret"));
}

#[tokio::test]
async fn debug_wraps_the_same_execution_with_actual_dynamic_queries_and_both_pinned_networks() {
    let mut implementation = captured();
    implementation["address"] = IMPLEMENTATION.into();
    implementation["matchId"] = "2".into();
    implementation["compilation"]["name"] = "LiveImplementation".into();
    let mock = Mock::with_records(vec![captured(), implementation]).await;
    let app = mock.with_ethereum().await;
    for query in [
        "?fields=all",
        "?fields=abi,compilation",
        "?omit=proxyResolution",
        "?fields=proxyResolution.onchainRuntimeBytecode",
    ] {
        let (normal_status, normal) = get(app.router(), &uri(query)).await;
        assert_eq!(normal_status, StatusCode::OK, "{normal}");
        assert!(normal.get("trace").is_none());
        mock.state.requests.lock().unwrap().clear();
        mock.state.ethereum_requests.lock().unwrap().clear();
        let (status, debug) = get(app.router(), &debug_uri(query)).await;
        assert_eq!(status, normal_status);
        assert_eq!(debug["response"], normal);
        assert_trace_matches_rpc(&mock, &debug);
        let calls = debug["trace"]["rpcCalls"].as_array().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call["network"] == "arkiv" && call["method"] == "eth_chainId")
                .count(),
            1
        );
        assert_eq!(calls.first().unwrap()["method"], "eth_chainId");
        assert_eq!(calls.last().unwrap()["network"], "arkiv");
        assert_eq!(calls.last().unwrap()["params"], json!(["0x2a", false]));
        if query == "?fields=all" {
            assert_eq!(
                debug["response"]["proxyResolution"]["implementations"][0]["name"],
                "LiveImplementation"
            );
            let storage = calls
                .iter()
                .find(|call| call["method"] == "eth_getStorageAt")
                .unwrap();
            assert_eq!(storage["network"], "ethereum");
            assert_eq!(
                storage["params"][2],
                json!({"blockHash":format!("0x{}", "55".repeat(32)),"requireCanonical":true})
            );
            assert!(calls
                .iter()
                .any(|call| call["network"] == "ethereum"
                    && call["params"] == json!(["0x99", false])));
            assert!(calls.iter().any(|call| call["method"] == "arkiv_query"
                && call["params"][0]
                    .as_str()
                    .unwrap()
                    .contains(&IMPLEMENTATION.to_ascii_lowercase())));
        } else {
            assert!(!calls.iter().any(|call| call["network"] == "ethereum"));
        }
    }
}

#[tokio::test]
async fn debug_concurrent_requests_have_isolated_addresses_and_rpc_counts() {
    let missing = "0x1111111111111111111111111111111111111111";
    let mock = Mock::new().await;
    let app = mock.with_ethereum().await;
    mock.state.requests.lock().unwrap().clear();
    mock.fault("slow-rpc");
    let found_uri = debug_uri("?fields=all");
    let absent_uri = format!("/v2/debug/contract/1/{missing}?fields=all");
    let normal_uri = uri("");
    let (found, absent, ordinary) = tokio::join!(
        get(app.router(), &found_uri),
        get(app.router(), &absent_uri),
        get(app.router(), &normal_uri),
    );
    assert_eq!(found.0, StatusCode::OK);
    assert_eq!(absent.0, StatusCode::NOT_FOUND);
    assert_eq!(ordinary.0, StatusCode::OK);
    assert!(ordinary.1.get("trace").is_none());
    let found_calls = found.1["trace"]["rpcCalls"].as_array().unwrap();
    let absent_calls = absent.1["trace"]["rpcCalls"].as_array().unwrap();
    assert_eq!(
        absent_calls.len(),
        5,
        "absence has only its own network/view/query/final check"
    );
    assert!(!found_calls
        .iter()
        .any(|call| call["params"].to_string().contains(missing)));
    assert!(!absent_calls
        .iter()
        .any(|call| call["params"].to_string().contains(ADDRESS)));
    assert!(!absent_calls
        .iter()
        .any(|call| call["network"] == "ethereum"));
    assert_eq!(
        found_calls
            .iter()
            .filter(|call| call["method"] == "eth_chainId")
            .count(),
        2
    );
    for calls in [found_calls, absent_calls] {
        for (index, call) in calls.iter().enumerate() {
            assert_eq!(call["index"], index);
        }
    }
    assert!(mock.state.peak.load(Ordering::Relaxed) > 1);
}

#[tokio::test]
async fn debug_preserves_validation_absence_backend_errors_and_nested_ethereum_errors() {
    let mock = Mock::new().await;
    let app = mock.with_ethereum().await;
    for query in ["?fields=nope", "?fields=abi&omit=sources", "?unknown=1"] {
        mock.state.requests.lock().unwrap().clear();
        let (status, debug) = get(app.router(), &debug_uri(query)).await;
        let (normal_status, normal) = get(app.router(), &uri(query)).await;
        assert_eq!(status, normal_status);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(debug["response"]["customCode"], normal["customCode"]);
        assert_eq!(debug["response"]["message"], normal["message"]);
        assert!(debug["trace"]["rpcCalls"].as_array().unwrap().is_empty());
        assert!(mock.state.requests.lock().unwrap().is_empty());
    }
    for fault in ["deleted", "rpc-error", "wrong-id", "reorg"] {
        mock.fault(fault);
        let (normal_status, normal) = get(app.router(), &uri("")).await;
        mock.state.requests.lock().unwrap().clear();
        let (status, debug) = get(app.router(), &debug_uri("")).await;
        assert_eq!(status, normal_status);
        if fault == "deleted" {
            assert_eq!(debug["response"], normal);
        } else {
            assert_eq!(debug["response"]["customCode"], normal["customCode"]);
        }
        assert_trace_matches_rpc(&mock, &debug);
        if matches!(fault, "rpc-error" | "wrong-id") {
            assert_eq!(debug["trace"]["rpcCalls"][0]["outcome"], "error");
        }
    }
    mock.fault("none");
    *mock.state.ethereum_fault.lock().unwrap() = true;
    mock.state.requests.lock().unwrap().clear();
    let (status, debug) = get(app.router(), &debug_uri("?fields=proxyResolution")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        debug["response"]["proxyResolution"]["proxyResolutionError"]["customCode"],
        "proxy_resolution_error"
    );
    assert_trace_matches_rpc(&mock, &debug);
    let failed = debug["trace"]["rpcCalls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|call| call["network"] == "ethereum")
        .unwrap();
    assert_eq!(failed["outcome"], "error");
}

#[tokio::test]
async fn debug_deadline_records_cancelled_transport_and_shares_admission_with_normal_reads() {
    let mock = Mock::new().await;
    let app = mock
        .application(Limits {
            max_concurrent_requests: 1,
            request_timeout: Duration::from_millis(500),
            ..Limits::default()
        })
        .await;
    mock.state.requests.lock().unwrap().clear();
    mock.fault("slow-query");
    let router = app.router();
    let task = tokio::spawn(async move { get(router, &debug_uri("")).await });
    mock.state.started.notified().await;
    let (overloaded, body) = get(app.router(), &debug_uri("")).await;
    assert_eq!(overloaded, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["response"]["customCode"], "overloaded");
    assert!(body["trace"]["rpcCalls"].as_array().unwrap().is_empty());
    assert_eq!(
        get(app.router(), &uri("")).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let (status, body) = task.await.unwrap();
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(body["response"]["customCode"], "deadline_exceeded");
    assert_trace_matches_rpc(&mock, &body);
    let last = body["trace"]["rpcCalls"]
        .as_array()
        .unwrap()
        .last()
        .unwrap();
    assert_eq!(last["method"], "arkiv_query");
    assert_eq!(last["outcome"], "cancelled");
    assert!(last["durationMs"].as_u64().unwrap() > 0);
    mock.fault("none");
    assert_eq!(get(app.router(), &uri("")).await.0, StatusCode::OK);
}
