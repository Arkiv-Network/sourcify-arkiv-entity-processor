//! Async transport driver around checked CPU hydration, live proxy resolution,
//! and implementation-name lookups within the original pinned Arkiv session.
use crate::{
    api::{self, ApiError},
    backend::{Backend, BackendError, ReadSession, UdcSession},
    ethereum::EthereumRpc,
    fields::{self, Selection},
    projection, Limits, Metrics,
};
use anyhow::{Context, Result};
use arkiv_entity_processor::{
    hydrate::hydrate_fields_with_check,
    udc::{Reader, Select, UdcEntity},
    Entity,
};
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
/// A v4 fetch the blocking hydration asks the async session for.
struct Fetch {
    keys: Vec<[u8; 32]>,
    select: Select,
    reply: oneshot::Sender<Result<Vec<UdcEntity>>>,
}
/// The open read: a v3 session, or a v4 one with its dataset's prefix and owner.
enum Session<'a> {
    V3(&'a mut dyn ReadSession),
    Udc(&'a mut dyn UdcSession, &'a str, [u8; 20]),
}
impl Session<'_> {
    /// The hydrated, validated and projected contract at `address`, if the dataset has one.
    async fn lookup(
        &mut self,
        address: &str,
        selection: Selection,
        context: &Hydration,
    ) -> Result<Option<Value>> {
        let lower = address.to_ascii_lowercase();
        match self {
            Session::V3(session) => match session.find_contract("1", &lower).await? {
                Some(contract) => Ok(Some(
                    hydrate(&mut **session, contract, address, selection, context).await?,
                )),
                None => Ok(None),
            },
            Session::Udc(session, prefix, owner) => match session.find_contract(&lower).await? {
                Some(contract) => Ok(Some(
                    hydrate_udc(
                        &mut **session,
                        prefix,
                        *owner,
                        contract,
                        address,
                        selection,
                        context,
                    )
                    .await?,
                )),
                None => Ok(None),
            },
        }
    }
    async fn finish(&mut self) -> Result<()> {
        match self {
            Session::V3(session) => session.finish().await,
            Session::Udc(session, ..) => session.finish().await,
        }
    }
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
    let (mut v3, mut v4);
    let mut session = match backend.udc() {
        None => {
            v3 = backend.begin_read_with_trace(trace.clone()).await?;
            Session::V3(v3.as_mut())
        }
        Some(udc) => {
            v4 = udc.begin_udc_read(trace.clone()).await?;
            Session::Udc(v4.as_mut(), udc.prefix(), udc.owner())
        }
    };
    metrics.lookups.fetch_add(1, Ordering::Relaxed);
    let resolve_proxy = selection.resolves_proxy();
    let Some(mut record) = session.lookup(address, selection, &context).await? else {
        session.finish().await?;
        return Err(api::not_found(address).into());
    };
    if resolve_proxy {
        let resolution =
            resolve_proxy_and_names(ethereum, &mut session, address, &record, &context).await;
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

/// v4 hydration: `udc::Reader` on a blocking worker, its key batches served by the async
/// session. Every fetched key counts as a dependency read.
async fn hydrate_udc(
    session: &mut dyn UdcSession,
    prefix: &str,
    owner: [u8; 20],
    contract: UdcEntity,
    address: &str,
    selection: Selection,
    context: &Hydration,
) -> Result<Value> {
    let (sender, mut fetches) = mpsc::channel::<Fetch>(1);
    let worker_context = context.clone();
    let address = address.to_owned();
    let prefix = prefix.to_owned();
    let mut worker = tokio::task::spawn_blocking(move || -> Result<Value> {
        let _permit = &worker_context.permit;
        let check = || worker_context.check();
        let fields = selection.dependencies();
        let mut fetch = |keys: &[[u8; 32]], select: &Select| -> Result<Vec<UdcEntity>> {
            check()?;
            let before = worker_context
                .reads
                .fetch_add(keys.len(), Ordering::Relaxed);
            anyhow::ensure!(
                before.saturating_add(keys.len()) <= worker_context.max_reads,
                BackendError::WorkLimit("dependency read limit exceeded".into())
            );
            worker_context
                .metrics
                .dependency_reads
                .fetch_add(keys.len() as u64, Ordering::Relaxed);
            let (reply, response) = oneshot::channel();
            sender
                .blocking_send(Fetch {
                    keys: keys.to_vec(),
                    select: select.clone(),
                    reply,
                })
                .map_err(|_| ApiError::deadline())?;
            response.blocking_recv().map_err(|_| ApiError::deadline())?
        };
        let record =
            Reader::batched(&prefix, Some(owner), &mut fetch).hydrate(&contract, Some(&fields))?;
        projection::validate(&record, "1", &address)?;
        check()?;
        let result = selection.project(record)?;
        check()?;
        Ok(result)
    });
    loop {
        tokio::select! {
            result = &mut worker => return result.context("hydration worker failed")?,
            request = fetches.recv() => match request {
                Some(request) => {
                    let result = session.fetch(&request.keys, &request.select).await;
                    let _ = request.reply.send(result);
                }
                None => return worker.await.context("hydration worker failed")?,
            }
        }
    }
}

async fn resolve_proxy_and_names(
    ethereum: Option<&EthereumRpc>,
    session: &mut Session<'_>,
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
        if let Some(contract) = session.lookup(&address, selection.clone(), context).await? {
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
