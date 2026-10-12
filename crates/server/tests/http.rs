use anyhow::Result;
use arkiv_entity_processor::{bundle::BundleError, transform, Entity};
use arkiv_sourcify_server::{
    backend::{Backend, ReadSession},
    fields, Application, Limits,
};
use axum::{
    body::{to_bytes, Body},
    http::{header, HeaderMap, Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use std::{
    io::Read,
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

async fn raw_request(
    router: Router,
    uri: &str,
    method: &str,
    accept_encoding: &[&str],
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut request = Request::builder().method(method).uri(uri);
    for value in accept_encoding {
        request = request.header(header::ACCEPT_ENCODING, *value);
    }
    let response = router
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, headers, bytes)
}

fn assert_encoding(headers: &HeaderMap, encoding: Option<&str>, bytes: &[u8]) {
    assert_eq!(
        headers
            .get(header::CONTENT_ENCODING)
            .map(|value| value.to_str().unwrap()),
        encoding
    );
    assert!(
        headers.get_all(header::VARY).iter().any(|value| {
            value
                .to_str()
                .unwrap()
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("accept-encoding"))
        }),
        "missing Vary: Accept-Encoding in {headers:?}"
    );
    if encoding.is_some() {
        assert!(
            !headers.contains_key(header::CONTENT_LENGTH),
            "compressed streaming body retained Content-Length"
        );
    } else if let Some(length) = headers.get(header::CONTENT_LENGTH) {
        assert_eq!(
            length.to_str().unwrap().parse::<usize>().unwrap(),
            bytes.len()
        );
    }
}

fn decode_body(encoding: Option<&str>, bytes: &[u8]) -> Vec<u8> {
    match encoding {
        None => bytes.to_vec(),
        Some("gzip") => {
            let mut decoded = Vec::new();
            flate2::read::GzDecoder::new(bytes)
                .read_to_end(&mut decoded)
                .unwrap();
            decoded
        }
        Some("zstd") => {
            let mut decoder = zstd::stream::read::Decoder::new(bytes).unwrap();
            // HTTP zstd frames must be decodable with an 8 MiB window, even
            // when the uncompressed response is larger than that window.
            decoder.window_log_max(23).unwrap();
            let mut decoded = Vec::new();
            decoder.read_to_end(&mut decoded).unwrap();
            decoded
        }
        Some(other) => panic!("unexpected encoding {other}"),
    }
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
    let router = app(backend).router();
    for encoding in [None, Some("gzip"), Some("zstd")] {
        let accepted = encoding.into_iter().collect::<Vec<_>>();
        let (status, headers, bytes) =
            raw_request(router.clone(), &uri("?fields=sources"), "GET", &accepted).await;
        assert_eq!(status, StatusCode::OK);
        assert_encoding(&headers, encoding, &bytes);
        let body: Value = serde_json::from_slice(&decode_body(encoding, &bytes)).unwrap();
        assert_eq!(body["sources"], record["sources"]);
    }
}

#[tokio::test]
async fn gzip_and_zstd_preserve_complete_fixture_response_bytes() {
    let record = captured();
    let router = app(Memory::new(&record, Fault::None)).router();
    let (status, headers, identity) =
        raw_request(router.clone(), &uri("?fields=all"), "GET", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_encoding(&headers, None, &identity);
    let body: Value = serde_json::from_slice(&identity).unwrap();
    assert_eq!(
        body,
        projection(&record, &fields::parse(Some("fields=all")).unwrap())
    );
    for encoding in ["gzip", "zstd"] {
        let (status, headers, encoded) =
            raw_request(router.clone(), &uri("?fields=all"), "GET", &[encoding]).await;
        assert_eq!(status, StatusCode::OK);
        assert_encoding(&headers, Some(encoding), &encoded);
        assert_eq!(headers[header::CONTENT_TYPE], "application/json");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(decode_body(Some(encoding), &encoded), identity);
        assert!(encoded.len() < identity.len());
        eprintln!(
            "fixture fields=all: identity={} bytes, {encoding}={} bytes",
            identity.len(),
            encoded.len()
        );
    }
}

#[tokio::test]
async fn accept_encoding_negotiates_quality_wildcards_and_repeated_headers() {
    let router = app(Memory::new(&captured(), Fault::None)).router();
    let cases: &[(&[&str], Option<&str>)] = &[
        (&[], None),
        (&[""], None),
        (&["identity"], None),
        (&["br"], None),
        (&["gzip"], Some("gzip")),
        (&["x-gzip"], Some("gzip")),
        (&["zstd"], Some("zstd")),
        (&["gzip, zstd"], Some("zstd")),
        (&["zstd, gzip"], Some("zstd")),
        (&["identity;q=1, gzip;q=1, zstd;q=1"], Some("zstd")),
        (&["identity;q=1, gzip;q=1"], Some("gzip")),
        (&["*"], Some("zstd")),
        (&["gzip;q=0"], None),
        (&["gzip;q=0, *;q=1"], Some("zstd")),
        (&["zstd;q=0, *;q=1"], Some("gzip")),
        (&["gzip;q=0.8, zstd;q=0.4, identity;q=0.9"], None),
        (&["gzip;q=0.8, zstd;q=0.4, identity;q=0.3"], Some("gzip")),
        (&["gzip;q=0.4, zstd;q=0.8, identity;q=0.3"], Some("zstd")),
        (&["gzip;q=0.1, identity;q=0"], Some("gzip")),
        (&["GZIP;q=1, ZSTD;q=0"], Some("gzip")),
        (&["gzip;q=0.6", "zstd;q=0.8"], Some("zstd")),
        (&["gzip;q=1", "gzip;q=0"], None),
        (&["gzip;q=1, gzip;q=0, zstd;q=0.5"], Some("zstd")),
        (&["zstd;q=0", "zstd;q=1", "gzip;q=0.5"], Some("gzip")),
    ];
    for (accepted, expected) in cases {
        let (status, headers, bytes) =
            raw_request(router.clone(), "/_ops/health", "GET", accepted).await;
        assert_eq!(status, StatusCode::OK, "{accepted:?}");
        assert_encoding(&headers, *expected, &bytes);
        let body: Value = serde_json::from_slice(&decode_body(*expected, &bytes)).unwrap();
        assert_eq!(body, json!({"status":"ok"}), "{accepted:?}");
    }
}

#[tokio::test]
async fn unacceptable_or_malformed_encodings_fail_before_backend_work() {
    let backend = Memory::new(&captured(), Fault::None);
    let router = app(backend.clone()).router();
    let rejected: &[(&[&str], StatusCode)] = &[
        (&["*;q=0"], StatusCode::NOT_ACCEPTABLE),
        (&["br, identity;q=0"], StatusCode::NOT_ACCEPTABLE),
        (
            &["gzip;q=0, zstd;q=0, identity;q=0"],
            StatusCode::NOT_ACCEPTABLE,
        ),
        (
            &["gzip;q=1", "gzip;q=0", "identity;q=0"],
            StatusCode::NOT_ACCEPTABLE,
        ),
        (
            &["identity;q=1", "identity;q=0"],
            StatusCode::NOT_ACCEPTABLE,
        ),
        (&["gzip;q=banana"], StatusCode::BAD_REQUEST),
        (&["gzip;q=2"], StatusCode::BAD_REQUEST),
        (&["zstd;q=-0.1"], StatusCode::BAD_REQUEST),
        (&["gzip;q=0.0001"], StatusCode::BAD_REQUEST),
        (&["zstd;q="], StatusCode::BAD_REQUEST),
        (&["gzip;q=0.5;q=0.8"], StatusCode::BAD_REQUEST),
        (&["br;q=banana"], StatusCode::BAD_REQUEST),
    ];
    for (accepted, expected) in rejected {
        let (status, headers, bytes) =
            raw_request(router.clone(), &uri("?fields=all"), "GET", accepted).await;
        assert_eq!(status, *expected, "{accepted:?}");
        assert!(
            bytes.is_empty(),
            "negotiation error returned a body for {accepted:?}"
        );
        assert_encoding(&headers, None, &bytes);
    }
    assert_eq!(backend.counts.lookups.load(Ordering::Relaxed), 0);
    assert!(backend.counts.dependencies.lock().unwrap().is_empty());
}

#[tokio::test]
async fn compression_preserves_json_error_status_and_head_has_no_body() {
    let backend = Memory::new(&captured(), Fault::None);
    let router = app(backend.clone()).router();
    for encoding in ["gzip", "zstd"] {
        for (path, expected, code) in [
            (
                uri("?fields=unknown"),
                StatusCode::BAD_REQUEST,
                "invalid_request",
            ),
            (
                "/v2/contract/1/0x1111111111111111111111111111111111111111".into(),
                StatusCode::NOT_FOUND,
                "contract_not_found",
            ),
        ] {
            let (status, headers, bytes) =
                raw_request(router.clone(), &path, "GET", &[encoding]).await;
            assert_eq!(status, expected);
            assert_encoding(&headers, Some(encoding), &bytes);
            let body: Value = serde_json::from_slice(&decode_body(Some(encoding), &bytes)).unwrap();
            assert_eq!(body["error"]["code"], code);
        }
        for (path, expected) in [
            ("/_ops/health".to_owned(), StatusCode::OK),
            (uri(""), StatusCode::METHOD_NOT_ALLOWED),
        ] {
            let (status, headers, bytes) =
                raw_request(router.clone(), &path, "HEAD", &[encoding]).await;
            assert_eq!(status, expected);
            assert!(bytes.is_empty());
            // HEAD may describe the negotiated GET representation, but must
            // never emit its body or retain an uncompressed Content-Length.
            if let Some(actual) = headers.get(header::CONTENT_ENCODING) {
                assert_eq!(actual, encoding);
                assert!(!headers.contains_key(header::CONTENT_LENGTH));
            }
            assert!(headers.get_all(header::VARY).iter().any(|value| value
                .to_str()
                .unwrap()
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("accept-encoding"))));
        }
    }
    assert_eq!(backend.counts.lookups.load(Ordering::Relaxed), 2);
    assert!(backend.counts.dependencies.lock().unwrap().is_empty());
}
