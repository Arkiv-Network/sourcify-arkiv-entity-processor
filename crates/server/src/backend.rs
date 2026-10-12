//! Synchronous, immutable fixture sessions, run exclusively on blocking workers.
//! An Arkiv adapter will need an asynchronous driver with one pinned read view;
//! do not put network I/O inside these synchronous resolver callbacks.
use anyhow::Result;
use arkiv_entity_processor::Entity;
use serde_json::Value;

pub trait ReadSession: Send {
    fn find_contract(&mut self, chain: &str, address: &str) -> Result<Option<Entity>>;
    fn resolve(&mut self, kind: &str, hash: &str, part: Option<u64>) -> Result<Entity>;
}

pub trait Backend: Send + Sync + 'static {
    fn begin_read(&self) -> Result<Box<dyn ReadSession>>;
    fn metadata(&self) -> Value;
}
