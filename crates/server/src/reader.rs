//! Async transport driver around checked CPU hydration, live proxy resolution,
//! and implementation-name lookups within the original pinned Arkiv session.
use crate::{
    api::{self, ApiError},
    backend::{Backend, ReadSession},
    ethereum::EthereumRpc,
    fields::{self, Selection},
    projection, Limits, Metrics,
};
use anyhow::{Context, Result};
use arkiv_entity_processor::{hydrate::hydrate_fields_with_check, Entity};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
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
#[derive(Clone)]
struct Hydration {
    deadline: Instant,
    cancel: Arc<AtomicBool>,
    reads: Arc<AtomicUsize>,
    max_reads: usize,
    metrics: Arc<Metrics>,
    permit: Arc<OwnedSemaphorePermit>,
    trace: Option<crate::trace::RpcTrace>,
}
impl Hydration {
    fn check(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) || Instant::now() >= self.deadline {
            return Err(ApiError::deadline().into());
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn read(
    backend: &dyn Backend,
    address: &str,
    selection: Selection,
    limits: &Limits,
    metrics: Arc<Metrics>,
    permit: OwnedSemaphorePermit,
    ethereum: Option<&EthereumRpc>,
    trace: Option<crate::trace::RpcTrace>,
) -> Result<Vec<u8>> {
    let cancellation = CancelOnDrop(Arc::new(AtomicBool::new(false)));
    let context = Hydration {
        deadline: Instant::now() + limits.request_timeout,
        cancel: cancellation.0.clone(),
        reads: Arc::new(AtomicUsize::new(0)),
        max_reads: limits.max_dependency_reads,
        metrics: metrics.clone(),
        permit: Arc::new(permit),
        trace: trace.clone(),
    };
    let mut session = backend.begin_read_with_trace(trace.clone()).await?;
    metrics.lookups.fetch_add(1, Ordering::Relaxed);
    let Some(contract) = session.find_contract("1", address).await? else {
        session.finish().await?;
        return Err(api::not_found(address).into());
    };
    let resolve_proxy = selection.resolves_proxy();
    let mut record = hydrate(session.as_mut(), contract, address, selection, &context).await?;
    if resolve_proxy {
        let resolution =
            resolve_proxy_and_names(ethereum, session.as_mut(), address, &record, &context).await;
        record["proxyResolution"] = match resolution {
            Ok(value) => value,
            Err(_) => json!({"proxyResolutionError": {
                "customCode":"proxy_resolution_error",
                "message":"Error while running proxy detection and implementation resolution",
                "errorId":uuid::Uuid::new_v4().to_string(),
            }}),
        };
    }
    context.check()?;
    session.finish().await?;
    // Serialize on a cancellable CPU worker, retaining global admission until
    // both the async driver and every worker have stopped.
    let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let _permit = &context.permit;
        context.check()?;
        let bytes = match trace {
            Some(trace) => serde_json::to_vec(&crate::trace::DebugResponse {
                response: record,
                trace: trace.snapshot(),
            })?,
            None => serde_json::to_vec(&record)?,
        };
        context.check()?;
        Ok(bytes)
    })
    .await
    .context("serialization worker failed")??;
    metrics
        .response_bytes
        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
    Ok(bytes)
}

async fn hydrate(
    session: &mut dyn ReadSession,
    contract: Entity,
    address: &str,
    selection: Selection,
    context: &Hydration,
) -> Result<Value> {
    let (sender, mut dependencies) = mpsc::channel::<Dependency>(1);
    let worker_context = context.clone();
    let address = address.to_owned();
    let mut worker = tokio::task::spawn_blocking(move || -> Result<Value> {
        let _permit = &worker_context.permit;
        let check = || worker_context.check();
        let record = hydrate_fields_with_check(
            &contract,
            &selection.dependencies(),
            |kind, hash, part| {
                check()?;
                anyhow::ensure!(
                    worker_context.reads.fetch_add(1, Ordering::Relaxed) < worker_context.max_reads,
                    crate::backend::BackendError::WorkLimit(
                        "dependency read limit exceeded".into()
                    )
                );
                worker_context
                    .metrics
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
        let result = selection.project(record)?;
        check()?;
        Ok(result)
    });
    loop {
        tokio::select! {
            result = &mut worker => return result.context("hydration worker failed")?,
            request = dependencies.recv() => match request {
                Some(request) => {
                    let result = session.resolve(&request.kind, &request.hash, request.part).await;
                    let _ = request.reply.send(result);
                }
                None => return worker.await.context("hydration worker failed")?,
            }
        }
    }
}

async fn resolve_proxy_and_names(
    ethereum: Option<&EthereumRpc>,
    session: &mut dyn ReadSession,
    address: &str,
    record: &Value,
    context: &Hydration,
) -> Result<Value> {
    let rpc = ethereum.context("Ethereum RPC is not configured")?;
    let code = record["proxyResolution"]["onchainCreationBytecode"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| record["proxyResolution"]["onchainRuntimeBytecode"].as_str())
        .context("proxy bytecode unavailable")?;
    let mut view = rpc.view_with_trace(context.trace.clone());
    let result = crate::proxy::resolve(&mut view, address, code).await?;
    anyhow::ensure!(
        result.implementations.len() <= rpc.max_implementations(),
        "proxy implementation limit exceeded"
    );
    let selection = fields::parse(Some("fields=compilation.name"))?;
    let mut implementations = Vec::new();
    for address in result.implementations {
        context.check()?;
        let address = api::checksum_address(&address).context("invalid implementation address")?;
        let mut implementation = json!({"address":address});
        context.metrics.lookups.fetch_add(1, Ordering::Relaxed);
        if let Some(contract) = session
            .find_contract("1", &address.to_ascii_lowercase())
            .await?
        {
            let contract = hydrate(session, contract, &address, selection.clone(), context).await?;
            if let Some(name) = contract["compilation"]["name"]
                .as_str()
                .filter(|s| !s.is_empty())
            {
                implementation["name"] = name.into();
            }
        }
        implementations.push(implementation);
    }
    view.finish().await?;
    Ok(
        json!({"isProxy":result.is_proxy,"proxyType":result.proxy_type,"implementations":implementations}),
    )
}
