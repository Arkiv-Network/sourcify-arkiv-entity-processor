use arkiv_entity_processor::{transform, Entity};
use arkiv_sourcify_server::{
    backend::{Backend, ReadFuture, ReadSession},
    ethereum::{Config, EthereumRpc},
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
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tower::ServiceExt;
const ADDRESS: &str = "0x000000000034065b3a94C2118CFe5B4C0067B615";
const IMPLEMENTATION: &str = "0x41675C099F32341bf84BFc5382aF534df5C7461a";
fn record() -> Value {
    serde_json::from_str(include_str!("../../../docs/data/sourcify-response.json")).unwrap()
}
fn reference(name: &str) -> Value {
    let all: Value = serde_json::from_str(include_str!("fixtures/proxy/references.json")).unwrap();
    all.as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == name)
        .unwrap()["body"]
        .clone()
}
#[derive(Clone)]
struct Memory {
    entities: Arc<Vec<Entity>>,
    sessions: Arc<AtomicUsize>,
    finish: Arc<AtomicUsize>,
    fail_name: Arc<AtomicBool>,
}
impl Memory {
    fn new(with_name: bool) -> Self {
        let mut entities = transform(&record()).unwrap();
        if with_name {
            let mut implementation = record();
            implementation["address"] = IMPLEMENTATION.into();
            implementation["matchId"] = "2".into();
            implementation["compilation"]["name"] = "Safe".into();
            entities.extend(transform(&implementation).unwrap());
        }
        Self {
            entities: Arc::new(entities),
            sessions: Arc::new(AtomicUsize::new(0)),
            finish: Arc::new(AtomicUsize::new(0)),
            fail_name: Arc::new(AtomicBool::new(false)),
        }
    }
}
impl Backend for Memory {
    fn begin_read(&self) -> ReadFuture<'_, Box<dyn ReadSession>> {
        Box::pin(async {
            self.sessions.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(self.clone()) as Box<dyn ReadSession>)
        })
    }
    fn metadata(&self) -> Value {
        json!({"backend":"test"})
    }
}
impl ReadSession for Memory {
    fn find_contract<'a>(
        &'a mut self,
        _: &'a str,
        address: &'a str,
    ) -> ReadFuture<'a, Option<Entity>> {
        Box::pin(async move {
            if self.fail_name.load(Ordering::Relaxed)
                && address.eq_ignore_ascii_case(IMPLEMENTATION)
            {
                anyhow::bail!("implementation lookup failed")
            }
            Ok(self
                .entities
                .iter()
                .find(|e| {
                    e.kind == "verified_contract"
                        && e.attributes["address"]["value"]
                            .as_str()
                            .is_some_and(|s| s.eq_ignore_ascii_case(address))
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
            self.entities
                .iter()
                .find(|e| {
                    e.kind == kind
                        && e.attributes["hash"]["value"] == hash
                        && part.is_none_or(|p| e.attributes["part"]["value"] == p)
                })
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("test dependency missing"))
        })
    }
    fn finish(&mut self) -> ReadFuture<'_, ()> {
        Box::pin(async {
            self.finish.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
    }
}
#[derive(Default)]
struct RpcState {
    requests: Mutex<Vec<Value>>,
    fault: Mutex<&'static str>,
    delay_ms: AtomicU64,
    active: AtomicUsize,
    peak: AtomicUsize,
}
async fn rpc(State(state): State<Arc<RpcState>>, Json(request): Json<Value>) -> Json<Value> {
    struct Active(Arc<RpcState>);
    impl Drop for Active {
        fn drop(&mut self) {
            self.0.active.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let active = state.active.fetch_add(1, Ordering::Relaxed) + 1;
    state.peak.fetch_max(active, Ordering::Relaxed);
    let _active = Active(state.clone());
    state.requests.lock().unwrap().push(request.clone());
    let delay = state.delay_ms.load(Ordering::Relaxed);
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    let fault = *state.fault.lock().unwrap();
    let result = match request["method"].as_str().unwrap() {
        "eth_chainId" => json!(if fault == "chain" { "0x2" } else { "0x1" }),
        "eth_getBlockByNumber" => {
            json!({"number":"0x99","hash":format!("0x{}", if fault == "reorg" && request["params"][0] != "latest" {"22"} else {"11"}.repeat(32))})
        }
        "eth_getStorageAt" => {
            assert_eq!(
                request["params"][2],
                json!({"blockHash":format!("0x{}","11".repeat(32)),"requireCanonical":true})
            );
            if fault == "storage" {
                json!("0x1")
            } else {
                json!(format!(
                    "0x{}{}",
                    "00".repeat(12),
                    &IMPLEMENTATION[2..].to_ascii_lowercase()
                ))
            }
        }
        "eth_call" => {
            assert_eq!(
                request["params"][1],
                json!({"blockHash":format!("0x{}","11".repeat(32)),"requireCanonical":true})
            );
            json!(if fault == "abi" {
                "0x".into()
            } else {
                format!("0x{:064x}{:064x}", 32, 0)
            })
        }
        method => panic!("unexpected RPC method {method}"),
    };
    if fault == "null-error" {
        return Json(json!({"jsonrpc":"2.0","id":request["id"],"error":null}));
    }
    if fault == "both" {
        return Json(
            json!({"jsonrpc":"2.0","id":request["id"],"result":result,"error":{"code":3,"message":"execution reverted"}}),
        );
    }
    if fault == "error" {
        return Json(
            json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":"provider failed"}}),
        );
    }
    Json(
        json!({"jsonrpc":"2.0","id": if fault == "id" {json!(9000)} else {request["id"].clone()},"result":result}),
    )
}
struct Mock {
    state: Arc<RpcState>,
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
        let state = Arc::new(RpcState::default());
        let router = Router::new()
            .route("/", post(rpc))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { state, url, server }
    }
    fn app(
        &self,
        backend: Memory,
        limits: Limits,
        configure: impl FnOnce(&mut Config),
    ) -> Application {
        let mut config = Config::new(self.url.clone());
        configure(&mut config);
        Application::with_ethereum(
            Arc::new(backend),
            limits,
            Some(EthereumRpc::new(config).unwrap()),
        )
        .unwrap()
    }
}
async fn get(router: Router, query: &str) -> (StatusCode, Value) {
    let response = router
        .oneshot(
            Request::builder()
                .uri(format!("/v2/contract/1/{ADDRESS}{query}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap(),
    )
}
fn assert_proxy_error(body: &Value) {
    let error = &body["proxyResolution"]["proxyResolutionError"];
    assert_eq!(error["customCode"], "proxy_resolution_error", "{body}");
    assert_eq!(
        error["message"],
        "Error while running proxy detection and implementation resolution"
    );
    assert_eq!(
        uuid::Uuid::parse_str(error["errorId"].as_str().unwrap())
            .unwrap()
            .get_version_num(),
        4
    );
}
#[tokio::test]
async fn proxy_matches_live_reference_names_use_same_arkiv_view_and_eth_is_pinned() {
    let mock = Mock::new().await;
    let backend = Memory::new(true);
    let app = mock.app(backend.clone(), Limits::default(), |_| {});
    for query in [
        "?fields=proxyResolution",
        "?fields=all",
        "?omit=abi",
        "?omit=proxyResolution.onchainRuntimeBytecode",
        "?fields=proxyResolution.onchainRuntimeBytecode,proxyResolution",
    ] {
        let (status, body) = get(app.router(), query).await;
        assert_eq!(status, 200);
        assert_eq!(
            body["proxyResolution"],
            reference("safe")["proxyResolution"]
        );
        if query == "?fields=proxyResolution" {
            assert_eq!(body, reference("safe"));
        }
    }
    assert_eq!(backend.sessions.load(Ordering::Relaxed), 5);
    assert_eq!(backend.finish.load(Ordering::Relaxed), 5);
    let requests = mock.state.requests.lock().unwrap();
    assert_eq!(requests.len(), 20);
    assert_eq!(requests[0]["method"], "eth_chainId");
    assert_eq!(requests[1]["params"], json!(["latest", false]));
    assert_eq!(requests[3]["params"], json!(["0x99", false]));
}
#[tokio::test]
async fn default_omit_and_raw_nested_selectors_make_no_ethereum_calls() {
    let mock = Mock::new().await;
    let app = mock.app(Memory::new(true), Limits::default(), |_| {});
    for query in [
        "",
        "?omit=proxyResolution",
        "?fields=proxyResolution.onchainRuntimeBytecode",
        "?fields=proxyResolution.onchainCreationBytecode",
    ] {
        let (status, body) = get(app.router(), query).await;
        assert_eq!(status, 200);
        if query.contains("onchainRuntimeBytecode") {
            assert_eq!(
                body["proxyResolution"],
                json!({"onchainRuntimeBytecode":record()["runtimeBytecode"]["onchainBytecode"]})
            );
        }
        if query.contains("onchainCreationBytecode") {
            assert_eq!(
                body["proxyResolution"],
                json!({"onchainCreationBytecode":record()["creationBytecode"]["onchainBytecode"]})
            );
        }
    }
    assert!(mock.state.requests.lock().unwrap().is_empty());
    let (status, _) = get(app.router(), "?fields=proxyResolution.isProxy").await;
    assert_eq!(status, 400);
}
#[tokio::test]
async fn failures_are_nested_but_missing_implementation_names_are_omitted() {
    let mock = Mock::new().await;
    let backend = Memory::new(false);
    let app = mock.app(backend, Limits::default(), |_| {});
    let (_, body) = get(app.router(), "?fields=proxyResolution").await;
    assert_eq!(
        body["proxyResolution"]["implementations"],
        json!([{"address":IMPLEMENTATION}])
    );
    for fault in ["chain", "storage", "reorg", "id", "error"] {
        *mock.state.fault.lock().unwrap() = fault;
        let (status, body) = get(app.router(), "?fields=proxyResolution").await;
        assert_eq!(status, 200);
        assert_proxy_error(&body);
    }
    *mock.state.fault.lock().unwrap() = "";
    let backend = Memory::new(true);
    backend.fail_name.store(true, Ordering::Relaxed);
    let app = mock.app(backend, Limits::default(), |_| {});
    assert_proxy_error(&get(app.router(), "?fields=proxyResolution").await.1);
    let no_rpc = Application::new(Arc::new(Memory::new(true)), Limits::default()).unwrap();
    assert_proxy_error(&get(no_rpc.router(), "?fields=proxyResolution").await.1);
}
#[tokio::test]
async fn ethereum_limits_deadlines_and_request_drop_bound_work() {
    let mock = Mock::new().await;
    for limit in ["calls", "bytes", "timeout"] {
        let app = mock.app(Memory::new(true), Limits::default(), |c| match limit {
            "calls" => c.max_rpc_requests = 3,
            "bytes" => c.max_rpc_response_bytes = 40,
            _ => c.rpc_timeout = Duration::from_millis(5),
        });
        if limit == "timeout" {
            mock.state.delay_ms.store(50, Ordering::Relaxed)
        }
        let (status, body) = get(app.router(), "?fields=proxyResolution").await;
        assert_eq!(status, 200);
        assert_proxy_error(&body);
    }
    mock.state.delay_ms.store(200, Ordering::Relaxed);
    let limits = Limits {
        max_concurrent_requests: 1,
        request_timeout: Duration::from_millis(30),
        ..Limits::default()
    };
    let app = mock.app(Memory::new(true), limits, |_| {});
    let (status, _) = get(app.router(), "?fields=proxyResolution").await;
    assert_eq!(status, 504);
    mock.state.requests.lock().unwrap().clear();
    let router = app.router();
    let task = tokio::spawn(async move { get(router, "?fields=proxyResolution").await });
    for _ in 0..100 {
        if !mock.state.requests.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(get(app.router(), "?fields=proxyResolution").await.0, 503);
    task.abort();
    let _ = task.await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let count = mock.state.requests.lock().unwrap().len();
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(mock.state.requests.lock().unwrap().len(), count);
    assert_eq!(get(app.router(), "").await.0, 200);
}

#[tokio::test]
async fn diamond_fallback_does_not_swallow_adapter_invariant_failures() {
    let mock = Mock::new().await;
    let mut diamond = record();
    diamond["creationBytecode"]["onchainBytecode"] =
        "0x7fc8fcad8db84d3cc18b4c41d551ea0ee66dd599cde068d998e57d5e09332c131b".into();
    let mut backend = Memory::new(false);
    backend.entities = Arc::new(transform(&diamond).unwrap());
    let app = mock.app(backend.clone(), Limits::default(), |_| {});
    for fault in ["chain", "id", "both", "null-error"] {
        *mock.state.fault.lock().unwrap() = fault;
        let (status, body) = get(app.router(), "?fields=proxyResolution").await;
        assert_eq!(status, 200);
        assert_proxy_error(&body);
    }
    // The pinned upstream resolver catches ordinary Diamond RPC/ABI failures.
    for fault in ["error", "abi"] {
        *mock.state.fault.lock().unwrap() = fault;
        let (status, body) = get(app.router(), "?fields=proxyResolution").await;
        assert_eq!(status, 200);
        assert_eq!(
            body["proxyResolution"],
            json!({"isProxy":false,"proxyType":null,"implementations":[]})
        );
    }
    *mock.state.fault.lock().unwrap() = "";
    let app = mock.app(backend, Limits::default(), |c| c.max_rpc_requests = 2);
    assert_proxy_error(&get(app.router(), "?fields=proxyResolution").await.1);
}

#[tokio::test]
async fn ethereum_rpc_gate_is_global_across_requests() {
    let mock = Mock::new().await;
    mock.state.delay_ms.store(5, Ordering::Relaxed);
    let app = mock.app(Memory::new(true), Limits::default(), |c| {
        c.max_rpc_concurrency = 1
    });
    let responses = futures_util::future::join_all(
        (0..4).map(|_| get(app.router(), "?fields=proxyResolution")),
    )
    .await;
    for (status, body) in responses {
        assert_eq!(status, 200);
        assert_eq!(body["proxyResolution"]["isProxy"], true);
    }
    assert_eq!(mock.state.peak.load(Ordering::Relaxed), 1);
}

#[test]
fn ethereum_startup_configuration_is_validated_without_exposing_url() {
    for url in [
        "not a url secret",
        "file:///secret",
        "ftp://host/secret",
        "https://host/#secret",
    ] {
        let error = match EthereumRpc::new(Config::new(url.into())) {
            Ok(_) => panic!("invalid URL accepted"),
            Err(error) => error.to_string(),
        };
        assert!(!error.contains("secret"));
    }
    for limit in 0..7 {
        let mut config = Config::new("https://host".into());
        match limit {
            0 => config.max_rpc_concurrency = 0,
            1 => config.max_rpc_requests = 0,
            2 => config.max_rpc_response_bytes = 0,
            3 => config.max_read_bytes = 0,
            4 => config.max_implementations = 0,
            5 => config.rpc_timeout = Duration::ZERO,
            _ => config.max_rpc_concurrency = usize::MAX,
        }
        assert!(EthereumRpc::new(config).is_err());
    }
}
