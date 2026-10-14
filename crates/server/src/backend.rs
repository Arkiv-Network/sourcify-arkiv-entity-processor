//! Async read sessions own one consistent Arkiv view. CPU hydration has no transport.
use anyhow::Result;
use arkiv_entity_processor::Entity;
use serde_json::Value;
use std::{fmt, future::Future, pin::Pin};

pub type ReadFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

pub trait ReadSession: Send {
    fn find_contract<'a>(
        &'a mut self,
        chain: &'a str,
        address: &'a str,
    ) -> ReadFuture<'a, Option<Entity>>;
    fn resolve<'a>(
        &'a mut self,
        kind: &'a str,
        hash: &'a str,
        part: Option<u64>,
    ) -> ReadFuture<'a, Entity>;
    /// Check the numeric block selector still denotes the same block/root before responding.
    fn finish(&mut self) -> ReadFuture<'_, ()>;
}

pub trait Backend: Send + Sync + 'static {
    fn begin_read(&self) -> ReadFuture<'_, Box<dyn ReadSession>>;
    /// Optional request-owned transport trace. Backends without RPC transport
    /// retain their ordinary behavior and produce no synthetic calls.
    fn begin_read_with_trace(
        &self,
        _trace: Option<crate::trace::RpcTrace>,
    ) -> ReadFuture<'_, Box<dyn ReadSession>> {
        self.begin_read()
    }
    fn metadata(&self) -> Value;
    fn check_ready(&self) -> ReadFuture<'_, ()> {
        Box::pin(async { self.begin_read().await.map(|_| ()) })
    }
}

#[derive(Debug)]
pub enum BackendError {
    Unavailable(String),
    MissingDependency(String),
    Ambiguous(String),
    Corrupt(String),
    WorkLimit(String),
    StateChanged(String),
}
impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, message) = match self {
            Self::Unavailable(s) => ("Arkiv unavailable", s),
            Self::MissingDependency(s) => ("missing dependency", s),
            Self::Ambiguous(s) => ("ambiguous lookup", s),
            Self::Corrupt(s) => ("corrupt Arkiv response", s),
            Self::WorkLimit(s) => ("Arkiv work limit", s),
            Self::StateChanged(s) => ("Arkiv read view changed", s),
        };
        write!(f, "{kind}: {message}")
    }
}
impl std::error::Error for BackendError {}
