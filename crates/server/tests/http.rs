use arkiv_entity_processor::{transform, Entity};
use arkiv_sourcify_server::{
    backend::{Backend, BackendError, ReadFuture, ReadSession},
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
    fn begin_read(&self) -> ReadFuture<'_, Box<dyn ReadSession>> {
        Box::pin(async {
            if matches!(self.fault, Fault::Unavailable) {
                return Err(BackendError::Unavailable("test outage".into()).into());
            }
            Ok(Box::new(self.clone()) as Box<dyn ReadSession>)
        })
    }
    fn metadata(&self) -> Value {
        json!({"backend":"test","fixture":"captured-mainnet-2026-10-09"})
    }
}
impl ReadSession for Memory {
    fn find_contract<'a>(
        &'a mut self,
        _: &'a str,
        address: &'a str,
    ) -> ReadFuture<'a, Option<Entity>> {
        Box::pin(async move {
            self.counts.lookups.fetch_add(1, Ordering::Relaxed);
            self.started.notify_one();
            if matches!(self.fault, Fault::Slow) {
                tokio::time::sleep(Duration::from_millis(120)).await;
            }
            if matches!(self.fault, Fault::Ambiguous) {
                return Err(BackendError::Ambiguous("test duplicates".into()).into());
            }
            if !address.eq_ignore_ascii_case(ADDRESS) {
                return Ok(None);
            }
            Ok(self
                .entities
                .iter()
                .find(|e| e.kind == "verified_contract")
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
            self.counts.dependencies.lock().unwrap().push(kind.into());
            if matches!(self.fault, Fault::Missing) {
                return Err(
                    BackendError::MissingDependency("test omitted dependency".into()).into(),
                );
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
        })
    }
    fn finish(&mut self) -> ReadFuture<'_, ()> {
        Box::pin(async { Ok(()) })
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

#[tokio::test]
async fn debug_envelope_uses_existing_compression_cors_errors_and_negotiation() {
    let router = app(Memory::new(&captured(), Fault::None)).router();
    for encoding in [None, Some("gzip"), Some("zstd")] {
        for (query, expected_status) in [
            ("?fields=abi,compilation", StatusCode::OK),
            ("?fields=unknown", StatusCode::BAD_REQUEST),
        ] {
            let path = format!("/v2/debug/contract/1/{ADDRESS}{query}");
            let (status, headers, bytes) = raw_request(
                router.clone(),
                &path,
                "GET",
                &encoding.into_iter().collect::<Vec<_>>(),
            )
            .await;
            assert_eq!(status, expected_status);
            assert_eq!(headers[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
            assert_eq!(headers[header::CACHE_CONTROL], "no-store");
            assert_encoding(&headers, encoding, &bytes);
            let debug: Value = serde_json::from_slice(&decode_body(encoding, &bytes)).unwrap();
            assert_eq!(
                debug["trace"],
                json!({"rpcCalls":[], "truncated":false, "omittedRpcCalls":0})
            );
            let (normal_status, normal) = request(router.clone(), &uri(query)).await;
            assert_eq!(status, normal_status);
            if status == StatusCode::OK {
                assert_eq!(debug["response"], normal);
            } else {
                assert_eq!(debug["response"]["customCode"], normal["customCode"]);
                assert_eq!(debug["response"]["message"], normal["message"]);
            }
        }
    }
    let path = format!("/v2/debug/contract/1/{ADDRESS}");
    let (status, _, bytes) = raw_request(router.clone(), &path, "HEAD", &["gzip"]).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(bytes.is_empty());
    let (status, _, bytes) = raw_request(router.clone(), &path, "POST", &[]).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["customCode"],
        "method_not_allowed"
    );
    let (status, _, bytes) = raw_request(router, &path, "GET", &["identity;q=0,*;q=0"]).await;
    assert_eq!(status, StatusCode::NOT_ACCEPTABLE);
    assert!(bytes.is_empty());
}
#[tokio::test]
async fn parquet_metadata_http_string_matches_independent_live_capture() {
    let original: Value = serde_json::from_str(include_str!(
        "fixtures/metadata-whitespace/mirror-before.json"
    ))
    .unwrap();
    let expected: Value =
        serde_json::from_str(include_str!("fixtures/metadata-whitespace/sourcify.json")).unwrap();
    assert_ne!(original["stdJsonOutput"], expected["stdJsonOutput"]);
    let backend = Memory::new(&original, Fault::None);
    let hydrate_original = || {
        let contract = backend
            .entities
            .iter()
            .find(|e| e.kind == "verified_contract")
            .unwrap();
        arkiv_entity_processor::hydrate::hydrate_with(contract, |kind, hash, part| {
            Ok(backend
                .entities
                .iter()
                .find(|e| {
                    e.kind == kind
                        && e.attributes["hash"]["value"] == hash
                        && part.is_none_or(|p| e.attributes["part"]["value"] == p)
                })
                .unwrap()
                .clone())
        })
        .unwrap()
    };
    assert_eq!(hydrate_original(), original);
    for query in ["?omit=proxyResolution", "?fields=stdJsonOutput"] {
        let (status, actual) = request(app(backend.clone()).router(), &uri(query)).await;
        assert_eq!(status, StatusCode::OK);
        if query == "?omit=proxyResolution" {
            // Full JSON equality compares the embedded metadata as an exact
            // string, not as a parsed inner JSON object.
            assert_eq!(actual, expected);
        } else {
            assert_eq!(actual["stdJsonOutput"], expected["stdJsonOutput"]);
            assert_eq!(actual.as_object().unwrap().len(), fields::MINIMAL.len() + 1);
        }
    }
    // Serving policy must not rewrite entities or offline reconstruction.
    assert_eq!(hydrate_original(), original);
}

#[tokio::test]
async fn captured_fixture_minimal_selected_and_all_preserve_values() {
    let record = captured();
    for query in [
        "",
        "?fields=abi,compilation",
        "?fields=sources",
        "?fields=stdJsonInput,stdJsonOutput",
        "?omit=proxyResolution",
    ] {
        let backend = Memory::new(&record, Fault::None);
        let (status, actual) = request(app(backend.clone()).router(), &uri(query)).await;
        assert_eq!(status, StatusCode::OK, "{query}: {actual}");
        let selected = fields::parse(query.strip_prefix('?')).unwrap();
        assert_eq!(actual, selected.project(record.clone()).unwrap(), "{query}");
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
        "?fields=unknown",
        "?fields=proxyResolution.isProxy",
        "?fields=abi&omit=sources",
    ] {
        let (status, _) = request(router.clone(), &uri(query)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
    }
    for path in [
        format!("/v2/contract/10/{ADDRESS}"),
        "/v2/contract/1/0xabc".into(),
    ] {
        assert_eq!(
            request(router.clone(), &path).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(backend.counts.lookups.load(Ordering::Relaxed), 0);
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
    assert_eq!(body["match"], Value::Null);
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
            &uri("?omit=proxyResolution"),
        )
        .await;
        assert_eq!(actual, status);
        assert_eq!(body["customCode"], code);
        assert!(body.get("errorId").is_some());
    }
}

#[tokio::test]
async fn absent_and_null_are_distinct_and_api_types_are_normalized() {
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
    assert_eq!(actual["chainId"], "1");
    assert_eq!(
        actual,
        fields::parse(Some("fields=metadata,storageLayout,signatures"))
            .unwrap()
            .project(record)
            .unwrap()
    );
}

#[tokio::test]
async fn operational_routes_and_shutdown_are_separate_from_contract_json() {
    let application = app(Memory::new(&captured(), Fault::None));
    let router = application.router();
    let (_, ready) = request(router.clone(), "/_ops/ready").await;
    assert_eq!(ready["mode"], "arkiv");
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
            .unwrap()["customCode"],
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
async fn timeout_cancels_async_lookup_and_releases_admission_permit() {
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
    let task =
        tokio::spawn(async move { request(pending_router, &uri("?omit=proxyResolution")).await });
    backend.started.notified().await;
    assert_eq!(task.await.unwrap().0, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        request(router.clone(), "/_ops/metrics").await.1["activeWorkers"],
        0
    );
    assert!(backend.counts.dependencies.lock().unwrap().is_empty());
    assert_eq!(request(router, "/_ops/metrics").await.1["activeWorkers"], 0);
}

#[tokio::test]
async fn dropping_request_stops_further_dependency_work() {
    let backend = Memory::new(&captured(), Fault::Slow);
    let router = app(backend.clone()).router();
    let task = tokio::spawn(async move { request(router, &uri("?omit=proxyResolution")).await });
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
    let (status, body) = request(application.router(), &uri("?omit=proxyResolution")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["customCode"], "work_limit");
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
        raw_request(router.clone(), &uri("?omit=proxyResolution"), "GET", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_encoding(&headers, None, &identity);
    let body: Value = serde_json::from_slice(&identity).unwrap();
    assert_eq!(
        body,
        fields::parse(Some("omit=proxyResolution"))
            .unwrap()
            .project(record.clone())
            .unwrap()
    );
    for encoding in ["gzip", "zstd"] {
        let (status, headers, encoded) = raw_request(
            router.clone(),
            &uri("?omit=proxyResolution"),
            "GET",
            &[encoding],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_encoding(&headers, Some(encoding), &encoded);
        assert_eq!(
            headers[header::CONTENT_TYPE],
            "application/json; charset=utf-8"
        );
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
        let (status, headers, bytes) = raw_request(
            router.clone(),
            &uri("?omit=proxyResolution"),
            "GET",
            accepted,
        )
        .await;
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
            if expected == StatusCode::NOT_FOUND {
                assert!(body["match"].is_null());
            } else {
                assert!(body["customCode"].is_string(), "{code}");
            }
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
