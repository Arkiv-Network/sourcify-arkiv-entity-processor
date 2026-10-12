use crate::backend::{Backend, ReadSession};
use anyhow::Result;
use arkiv_entity_processor::{bundle::Bundle, Entity};
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};

pub struct BundleBackend(Arc<Bundle>);
impl BundleBackend {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        Ok(Self(Arc::new(Bundle::open(dir)?)))
    }
}
impl Backend for BundleBackend {
    fn begin_read(&self) -> Result<Box<dyn ReadSession>> {
        Ok(Box::new(BundleSession(self.0.clone())))
    }
    fn metadata(&self) -> Value {
        let m = self.0.metadata();
        json!({"backend":"bundle", "manifestSha256":m.manifest_sha256, "seedSha256":m.seed_sha256,
            "schemaVersion":m.schema_version, "arkivChainId":m.arkiv_chain_id, "sourceChainId":m.source_chain_id,
            "contracts":m.contracts, "inputContracts":m.input_contracts, "entities":m.entities,
            "parts":m.parts, "sourceCompleteRequired":m.source_complete_required})
    }
}
struct BundleSession(Arc<Bundle>);
impl ReadSession for BundleSession {
    fn find_contract(&mut self, chain: &str, address: &str) -> Result<Option<Entity>> {
        self.0.find_contract(chain, address)
    }
    fn resolve(&mut self, kind: &str, hash: &str, part: Option<u64>) -> Result<Entity> {
        self.0.resolve(kind, hash, part)
    }
}
