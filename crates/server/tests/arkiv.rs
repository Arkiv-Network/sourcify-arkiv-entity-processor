//! Protocol tests exercise real async HTTP JSON-RPC, without a bundle adapter.
use arkiv_entity_processor::{entity_key, hx, sha, transform, Entity};
use arkiv_sourcify_server::{
    backends::arkiv::{ArkivBackend, Config},
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
        let state = Arc::new(MockState {
            entities: transform(&record).unwrap().iter().map(rpc_entity).collect(),
            fault: Mutex::new("none"),
            requests: Mutex::new(Vec::new()),
            started: tokio::sync::Notify::new(),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/", post(rpc))
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
                            && query.contains(&format!("address = addr({ADDRESS})"));
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
