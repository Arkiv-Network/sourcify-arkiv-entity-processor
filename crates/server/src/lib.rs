//! The first fixture-backed read server. Production Arkiv transport is deferred.
pub mod api;
pub mod backend;
pub mod backends;
mod compression;
pub mod fields;
mod projection;
mod reader;

use axum::{
    extract::{Path, RawQuery, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use backend::Backend;
use serde_json::json;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::Semaphore;

#[derive(Clone, Debug)]
pub struct Limits {
    pub max_concurrent_requests: usize,
    pub request_timeout: Duration,
    pub max_dependency_reads: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_concurrent_requests: 8,
            request_timeout: Duration::from_secs(120),
            max_dependency_reads: 100_000,
        }
    }
}

#[derive(Default)]
struct Metrics {
    requests: AtomicU64,
    successes: AtomicU64,
    failures: AtomicU64,
    lookups: AtomicU64,
    dependency_reads: AtomicU64,
    response_bytes: AtomicU64,
}
struct AppState {
    backend: Arc<dyn Backend>,
    limits: Limits,
    slots: Arc<Semaphore>,
    ready: AtomicBool,
    metrics: Metrics,
}
#[derive(Clone)]
pub struct Application {
    state: Arc<AppState>,
}
impl Application {
    pub fn new(backend: Arc<dyn Backend>, limits: Limits) -> anyhow::Result<Self> {
        anyhow::ensure!(
            limits.max_concurrent_requests > 0
                && limits.max_concurrent_requests <= Semaphore::MAX_PERMITS
                && limits.max_dependency_reads > 0
                && !limits.request_timeout.is_zero()
                && std::time::Instant::now()
                    .checked_add(limits.request_timeout)
                    .is_some(),
            "server limits must be positive and representable"
        );
        Ok(Self {
            state: Arc::new(AppState {
                backend,
                slots: Arc::new(Semaphore::new(limits.max_concurrent_requests)),
                limits,
                ready: AtomicBool::new(true),
                metrics: Metrics::default(),
            }),
        })
    }
    pub fn router(&self) -> Router {
        Router::new()
            .route(
                "/v2/contract/{chainId}/{address}",
                get(contract).head(method_not_allowed),
            )
            .route(
                "/_ops/health",
                get(|| async { Json(json!({"status":"ok"})) }),
            )
            .route("/_ops/ready", get(ready))
            .route("/_ops/metrics", get(metrics))
            .fallback(|| async {
                api::ApiError::new(StatusCode::NOT_FOUND, "route_not_found", "Unknown route")
            })
            .method_not_allowed_fallback(method_not_allowed)
            .with_state(self.state.clone())
            .layer(
                tower_http::compression::CompressionLayer::new().compress_when(compression::Json),
            )
            .layer(axum::middleware::from_fn(compression::negotiate))
    }
    /// Called before draining accepted requests on SIGINT/SIGTERM.
    pub fn shutdown(&self) {
        self.state.ready.store(false, Ordering::Relaxed);
    }
}

async fn method_not_allowed() -> api::ApiError {
    api::ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "This route supports GET only",
    )
}
async fn ready(State(state): State<Arc<AppState>>) -> Response {
    if !state.ready.load(Ordering::Relaxed) {
        return api::ApiError::unavailable().into_response();
    }
    Json(json!({"status":"ready", "mode":"fixture", "apiContract":"fixture-v1", "dataset":state.backend.metadata()})).into_response()
}
async fn metrics(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let m = &state.metrics;
    Json(
        json!({"requests": m.requests.load(Ordering::Relaxed), "successes":m.successes.load(Ordering::Relaxed), "failures":m.failures.load(Ordering::Relaxed), "lookups":m.lookups.load(Ordering::Relaxed), "dependencyReads":m.dependency_reads.load(Ordering::Relaxed), "responseBytes":m.response_bytes.load(Ordering::Relaxed), "activeWorkers":state.limits.max_concurrent_requests - state.slots.available_permits()}),
    )
}
async fn contract(
    State(state): State<Arc<AppState>>,
    path: Result<Path<(String, String)>, axum::extract::rejection::PathRejection>,
    RawQuery(query): RawQuery,
) -> Response {
    state.metrics.requests.fetch_add(1, Ordering::Relaxed);
    let result = contract_inner(&state, path, query).await;
    match result {
        Ok(bytes) => {
            state.metrics.successes.fetch_add(1, Ordering::Relaxed);
            (
                [
                    (header::CONTENT_TYPE, "application/json"),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                bytes,
            )
                .into_response()
        }
        Err(error) => {
            state.metrics.failures.fetch_add(1, Ordering::Relaxed);
            error.into_response()
        }
    }
}
async fn contract_inner(
    state: &Arc<AppState>,
    path: Result<Path<(String, String)>, axum::extract::rejection::PathRejection>,
    query: Option<String>,
) -> Result<Vec<u8>, api::ApiError> {
    let Path((chain, address)) = path.map_err(|_| api::ApiError::invalid("Invalid URL path"))?;
    let address = api::validate_path(&chain, &address)?;
    let fields = fields::parse(query.as_deref())?;
    if !state.ready.load(Ordering::Relaxed) {
        return Err(api::ApiError::unavailable());
    }
    let permit = state.slots.clone().try_acquire_owned().map_err(|_| {
        api::ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded",
            "All contract read workers are busy",
        )
    })?;
    let cancellation = reader::CancelOnDrop(Arc::new(AtomicBool::new(false)));
    let cancel = cancellation.0.clone();
    let worker_state = state.clone();
    let worker = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        reader::read(
            worker_state.backend.as_ref(),
            &address,
            &fields,
            &worker_state.limits,
            &cancel,
            &worker_state.metrics,
        )
    });
    tokio::time::timeout(state.limits.request_timeout, worker)
        .await
        .map_err(|_| api::ApiError::deadline())?
        .map_err(|error| {
            tracing::error!(%error, "read worker failed");
            api::ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "The read worker failed",
            )
        })?
        .map_err(api::ApiError::from_read)
}
