use anyhow::Result;
use arkiv_entity_processor::{bundle::BundleError, transform, Entity};
use arkiv_sourcify_server::{
    backend::{Backend, ReadSession},
    fields, Application, Limits,
};
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
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

const ADDRESS: &str = "0x000000000034065b3a94C2118CFe5B4C0067B615";
fn captured() -> Value {
    serde_json::from_str(include_str!("../../../docs/data/sourcify-response.json")).unwrap()
}
#[derive(Clone, Copy, Default)]
enum Fault {
    #[default]
    None,
    Unavailable,
    Missing,
    Corrupt,
    Ambiguous,
    Slow,
}
#[derive(Default)]
struct Counts {
    lookups: AtomicUsize,
    dependencies: Mutex<Vec<String>>,
}
#[derive(Clone)]
struct Memory {
    entities: Arc<Vec<Entity>>,
    counts: Arc<Counts>,
    started: Arc<tokio::sync::Notify>,
    fault: Fault,
}
impl Memory {
    fn new(record: &Value, fault: Fault) -> Self {
        Self {
            entities: Arc::new(transform(record).unwrap()),
            counts: Arc::new(Counts::default()),
            started: Arc::new(tokio::sync::Notify::new()),
            fault,
        }
    }
}
impl Backend for Memory {
    fn begin_read(&self) -> Result<Box<dyn ReadSession>> {
        if matches!(self.fault, Fault::Unavailable) {
            return Err(BundleError::Unavailable("fixture outage".into()).into());
        }
        Ok(Box::new(self.clone()))
    }
    fn metadata(&self) -> Value {
        json!({"backend":"test","fixture":"captured-mainnet-2026-10-09"})
    }
}
impl ReadSession for Memory {
    fn find_contract(&mut self, _: &str, address: &str) -> Result<Option<Entity>> {
        self.counts.lookups.fetch_add(1, Ordering::Relaxed);
        self.started.notify_one();
        if matches!(self.fault, Fault::Slow) {
            std::thread::sleep(Duration::from_millis(120));
        }
        if matches!(self.fault, Fault::Ambiguous) {
            return Err(BundleError::Ambiguous("fixture duplicates".into()).into());
        }
        if !address.eq_ignore_ascii_case(ADDRESS) {
            return Ok(None);
        }
        Ok(self
            .entities
            .iter()
            .find(|e| e.kind == "verified_contract")
            .cloned())
    }
    fn resolve(&mut self, kind: &str, hash: &str, part: Option<u64>) -> Result<Entity> {
        self.counts.dependencies.lock().unwrap().push(kind.into());
        if matches!(self.fault, Fault::Missing) {
            return Err(BundleError::MissingDependency("fixture omitted dependency".into()).into());
        }
        let mut entity = self
            .entities
            .iter()
            .find(|e| {
                e.kind == kind
                    && e.attributes["hash"]["value"] == hash
                    && part.is_none_or(|p| e.attributes["part"]["value"] == p)
            })
            .unwrap()
            .clone();
        if matches!(self.fault, Fault::Corrupt) {
            entity.payload.push(b'!');
        }
        Ok(entity)
    }
}
fn app(backend: Memory) -> Application {
    Application::new(Arc::new(backend), Limits::default()).unwrap()
}
fn uri(query: &str) -> String {
    format!("/v2/contract/1/{ADDRESS}{query}")
}
async fn request(router: Router, uri: &str) -> (StatusCode, Value) {
    let response = router
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
fn projection(record: &Value, selected: &[&str]) -> Value {
    Value::Object(
        selected
            .iter()
            .filter_map(|field| record.get(*field).map(|v| ((*field).into(), v.clone())))
            .collect(),
    )
}

#[tokio::test]
async fn captured_fixture_minimal_selected_and_all_preserve_values() {
    let record = captured();
    for query in [
        "",
        "?fields=abi,compilation",
        "?fields=sources",
        "?fields=stdJsonInput,stdJsonOutput",
        "?fields=all",
    ] {
        let backend = Memory::new(&record, Fault::None);
        let (status, actual) = request(app(backend.clone()).router(), &uri(query)).await;
        assert_eq!(status, StatusCode::OK, "{query}: {actual}");
        let selected = fields::parse(query.strip_prefix('?')).unwrap();
        assert_eq!(actual, projection(&record, &selected), "{query}");
        assert!(actual.get("proxyResolution").is_none());
        if query.is_empty() {
            assert!(backend.counts.dependencies.lock().unwrap().is_empty());
        }
        if query == "?fields=abi,compilation" {
            let reads = backend.counts.dependencies.lock().unwrap();
            assert_eq!(*reads, ["compilation"]);
        }
    }
}

#[tokio::test]
async fn request_validation_precedes_backend_reads() {
    let backend = Memory::new(&captured(), Fault::None);
    let router = app(backend.clone()).router();
    for query in [
        "?fields=",
        "?fields=abi,",
        "?fields=all,abi",
        "?fields=abi&fields=compilation",
        "?fields=abi&",
        "?fields=abi&omit=sources",
        "?omit=sources",
        "?fields=abi.name",
        "?fields=unknown",
        "?fields=proxyResolution",
        "?fields=proxyResolution.isProxy",
        "?fields=%GG",
        "?fields=%FF",
        "?fields",
        "?atBlock=latest",
    ] {
        let (status, body) = request(router.clone(), &uri(query)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert_eq!(body["error"]["code"], "invalid_request");
    }
    for path in [
        format!("/v2/contract/01/{ADDRESS}"),
        format!("/v2/contract/10/{ADDRESS}"),
        "/v2/contract/1/0xabc".into(),
        format!("/v2/contract/1/{}", ADDRESS.replace('0', "g")),
    ] {
        assert_eq!(
            request(router.clone(), &path).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(backend.counts.lookups.load(Ordering::Relaxed), 0);
    for query in [
        "?fields=abi,abi",
        "?fields=abi%2Ccompilation",
        "?fields=address,abi",
    ] {
        assert_eq!(request(router.clone(), &uri(query)).await.0, StatusCode::OK);
    }
    assert_eq!(
        request(router, &uri("").to_ascii_lowercase()).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn missing_is_one_lookup_and_no_dependencies() {
    let backend = Memory::new(&captured(), Fault::None);
    let (status, body) = request(
        app(backend.clone()).router(),
        "/v2/contract/1/0x1111111111111111111111111111111111111111?fields=all",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "contract_not_found");
    assert_eq!(backend.counts.lookups.load(Ordering::Relaxed), 1);
    assert!(backend.counts.dependencies.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unavailable_missing_corrupt_and_ambiguous_never_become_404_or_partial_success() {
    for (fault, status, code) in [
        (
            Fault::Unavailable,
            StatusCode::SERVICE_UNAVAILABLE,
            "backend_unavailable",
        ),
        (
            Fault::Missing,
            StatusCode::BAD_GATEWAY,
            "missing_dependency",
        ),
        (Fault::Corrupt, StatusCode::BAD_GATEWAY, "corrupt_data"),
        (
            Fault::Ambiguous,
            StatusCode::BAD_GATEWAY,
            "ambiguous_record",
        ),
    ] {
        let (actual, body) = request(
            app(Memory::new(&captured(), fault)).router(),
            &uri("?fields=all"),
        )
        .await;
        assert_eq!(actual, status);
        assert_eq!(body["error"]["code"], code);
        assert_eq!(body.as_object().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn absent_and_null_are_distinct_and_stored_signatures_are_preserved() {
    let mut record = captured();
    record.as_object_mut().unwrap().remove("metadata");
    record["storageLayout"] = Value::Null;
    record["chainId"] = json!(1);
    record["signatures"] = json!({"function":[{"signature":"stored_export_only()"}]});
    let (status, actual) = request(
        app(Memory::new(&record, Fault::None)).router(),
        &uri("?fields=metadata,storageLayout,signatures"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(actual.get("metadata").is_none());
    assert_eq!(actual.get("storageLayout"), Some(&Value::Null));
    assert_eq!(actual["chainId"], 1);
    assert_eq!(actual["signatures"], record["signatures"]);
}

#[tokio::test]
async fn operational_routes_and_shutdown_are_separate_from_contract_json() {
    let application = app(Memory::new(&captured(), Fault::None));
    let router = application.router();
    let (_, ready) = request(router.clone(), "/_ops/ready").await;
    assert_eq!(ready["mode"], "fixture");
    let (_, body) = request(router.clone(), &uri("")).await;
    assert!(body.get("dataset").is_none());
    assert_eq!(
        request(router.clone(), "/v2/contracts/1").await.0,
        StatusCode::NOT_FOUND
    );
    let post = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri(""))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(post.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        serde_json::from_slice::<Value>(&to_bytes(post.into_body(), usize::MAX).await.unwrap())
            .unwrap()["error"]["code"],
        "method_not_allowed"
    );
    application.shutdown();
    assert_eq!(
        request(router.clone(), "/_ops/ready").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        request(router.clone(), &uri("")).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(request(router, "/_ops/health").await.0, StatusCode::OK);
}

#[tokio::test]
async fn timeout_retains_worker_permit_until_cooperative_cancellation_finishes() {
    let backend = Memory::new(&captured(), Fault::Slow);
    let application = Application::new(
        Arc::new(backend.clone()),
        Limits {
            max_concurrent_requests: 1,
            request_timeout: Duration::from_millis(25),
            ..Limits::default()
        },
    )
    .unwrap();
    let router = application.router();
    let pending_router = router.clone();
    let task = tokio::spawn(async move { request(pending_router, &uri("?fields=all")).await });
    backend.started.notified().await;
    assert_eq!(task.await.unwrap().0, StatusCode::GATEWAY_TIMEOUT);
    let (status, body) = request(router.clone(), &uri("")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "overloaded");
    tokio::time::sleep(Duration::from_millis(160)).await;
    assert!(backend.counts.dependencies.lock().unwrap().is_empty());
    assert_eq!(request(router, "/_ops/metrics").await.1["activeWorkers"], 0);
}

#[tokio::test]
async fn dropping_request_stops_further_dependency_work() {
    let backend = Memory::new(&captured(), Fault::Slow);
    let router = app(backend.clone()).router();
    let task = tokio::spawn(async move { request(router, &uri("?fields=all")).await });
    backend.started.notified().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::sleep(Duration::from_millis(160)).await;
    assert!(backend.counts.dependencies.lock().unwrap().is_empty());
}

#[tokio::test]
async fn dependency_work_budget_fails_without_partial_body() {
    let backend = Memory::new(&captured(), Fault::None);
    let application = Application::new(
        Arc::new(backend.clone()),
        Limits {
            max_dependency_reads: 1,
            ..Limits::default()
        },
    )
    .unwrap();
    let (status, body) = request(application.router(), &uri("?fields=all")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "work_limit");
    assert_eq!(backend.counts.dependencies.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_large_single_selected_field_is_returned_completely() {
    let mut record = captured();
    // A 9 MiB source exceeds common middleware response caps; it is one selector.
    record["sources"] = json!({"large/~source.sol":{"content":"x".repeat(9 * 1024 * 1024)}});
    let backend = Memory::new(&record, Fault::None);
    let (status, body) = request(app(backend).router(), &uri("?fields=sources")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["sources"], record["sources"]);
}
