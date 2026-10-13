//! Async transport driver around the pure, checked CPU hydrator.
//! A bounded channel carries one unresolved dependency. RPC futures live here and
//! are cancelled on request drop; CPU workers retain permits until they stop.
use crate::{
    api::{self, ApiError},
    backend::Backend,
    fields::Selection,
    projection, Limits, Metrics,
};
use anyhow::Result;
use arkiv_entity_processor::{hydrate::hydrate_fields_with_check, Entity};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit};

pub(crate) struct CancelOnDrop(pub Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
struct Dependency {
    kind: String,
    hash: String,
    part: Option<u64>,
    reply: oneshot::Sender<Result<Entity>>,
}

pub(crate) async fn read(
    backend: &dyn Backend,
    address: &str,
    selection: Selection,
    limits: &Limits,
    metrics: Arc<Metrics>,
    permit: OwnedSemaphorePermit,
) -> Result<Vec<u8>> {
    let permit = Arc::new(permit);
    let deadline = Instant::now() + limits.request_timeout;
    let cancellation = CancelOnDrop(Arc::new(AtomicBool::new(false)));
    let mut session = backend.begin_read().await?;
    metrics.lookups.fetch_add(1, Ordering::Relaxed);
    let Some(contract) = session.find_contract("1", address).await? else {
        session.finish().await?;
        return Err(api::not_found(address).into());
    };
    let (sender, mut dependencies) = mpsc::channel::<Dependency>(1);
    let cancel = cancellation.0.clone();
    let fields = selection.dependencies();
    let max_dependency_reads = limits.max_dependency_reads;
    let worker_metrics = metrics.clone();
    let address = address.to_owned();
    let worker_permit = permit.clone();
    let mut worker = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let _permit = worker_permit;
        let check = || -> Result<()> {
            if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
                return Err(ApiError::deadline().into());
            }
            Ok(())
        };
        let mut reads = 0;
        let record = hydrate_fields_with_check(
            &contract,
            &fields,
            |kind, hash, part| {
                check()?;
                anyhow::ensure!(
                    reads < max_dependency_reads,
                    crate::backend::BackendError::WorkLimit(
                        "dependency read limit exceeded".into()
                    )
                );
                reads += 1;
                worker_metrics
                    .dependency_reads
                    .fetch_add(1, Ordering::Relaxed);
                let (reply, response) = oneshot::channel();
                sender
                    .blocking_send(Dependency {
                        kind: kind.into(),
                        hash: hash.into(),
                        part,
                        reply,
                    })
                    .map_err(|_| ApiError::deadline())?;
                response.blocking_recv().map_err(|_| ApiError::deadline())?
            },
            check,
        )?;
        projection::validate(&record, "1", &address)?;
        check()?;
        let record = selection.project(record)?;
        // Complete JSON serialization precedes any HTTP success headers.
        let encoded = serde_json::to_vec(&record)?;
        check()?;
        Ok(encoded)
    });
    loop {
        tokio::select! {
            result = &mut worker => {
                let bytes = result.map_err(|_| anyhow::anyhow!("hydration worker failed"))??;
                session.finish().await?;
                metrics.response_bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                return Ok(bytes)
            }
            request = dependencies.recv() => {
                match request {
                    Some(request) => {
                        let result = session.resolve(&request.kind, &request.hash, request.part).await;
                        // On error let the worker report it and retain its permit until exit.
                        let _ = request.reply.send(result);
                    }
                    None => {
                        // Sender drops when hydration finishes, possibly before JoinHandle wakes.
                        let bytes = worker.await.map_err(|_| anyhow::anyhow!("hydration worker failed"))??;
                        session.finish().await?;
                        metrics.response_bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                        return Ok(bytes)
                    }
                }
            }
        }
    }
}
