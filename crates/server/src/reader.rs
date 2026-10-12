use crate::{api::ApiError, backend::Backend, projection, Limits, Metrics};
use anyhow::Result;
use arkiv_entity_processor::hydrate::hydrate_fields_with_check;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};

/// Setting this flag on future drop cooperatively cancels a blocking fixture read.
/// The worker retains its concurrency permit until it actually stops.
pub(crate) struct CancelOnDrop(pub Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub(crate) fn read(
    backend: &dyn Backend,
    address: &str,
    fields: &[&str],
    limits: &Limits,
    cancel: &AtomicBool,
    metrics: &Metrics,
) -> Result<Vec<u8>> {
    let deadline = Instant::now() + limits.request_timeout;
    let check = || -> Result<()> {
        if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return Err(ApiError::deadline().into());
        }
        Ok(())
    };
    check()?;
    let mut session = backend.begin_read()?;
    metrics.lookups.fetch_add(1, Ordering::Relaxed);
    let contract = session.find_contract("1", address)?.ok_or_else(|| {
        ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "contract_not_found",
            "Contract is absent from the configured fixture dataset",
        )
    })?;
    check()?;
    let mut reads = 0;
    let record = hydrate_fields_with_check(
        &contract,
        fields,
        |kind, hash, part| {
            check()?;
            if reads >= limits.max_dependency_reads {
                return Err(ApiError::new(
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "work_limit",
                    "The configured dependency work limit was exceeded",
                )
                .into());
            }
            reads += 1;
            metrics.dependency_reads.fetch_add(1, Ordering::Relaxed);
            session.resolve(kind, hash, part)
        },
        check,
    )?;
    projection::validate(&record, "1", address)?;
    check()?;
    // Complete serialization happens before headers; errors never yield partial JSON.
    // Field count is not a byte limit. Large legitimate records remain supported.
    let encoded = serde_json::to_vec(&record)?;
    check()?;
    metrics
        .response_bytes
        .fetch_add(encoded.len() as u64, Ordering::Relaxed);
    Ok(encoded)
}
