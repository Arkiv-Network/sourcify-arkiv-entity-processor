use anyhow::Context;
use arkiv_sourcify_server::{
    backend::Backend,
    backends::{
        arkiv::{ArkivBackend, Config},
        udc::{Config as UdcConfig, UdcBackend, View},
    },
    ethereum::{Config as EthereumConfig, EthereumRpc},
    Application, Limits,
};
use clap::Parser;
use std::{net::SocketAddr, sync::Arc, time::Duration};

#[derive(Parser)]
#[command(version, about = "Arkiv-backed Sourcify Mainnet contract reader")]
struct Args {
    #[arg(long)]
    rpc_url: String,
    /// Optional Mainnet RPC for live proxy resolution; never used for Arkiv reads
    #[arg(long)]
    ethereum_rpc_url: Option<String>,
    #[arg(long, default_value_t = 16)]
    max_ethereum_rpc_concurrency: usize,
    #[arg(long, default_value_t = 64)]
    max_ethereum_rpc_requests: usize,
    #[arg(long, default_value_t = 15)]
    ethereum_rpc_timeout_seconds: u64,
    #[arg(long, default_value_t = 1024 * 1024)]
    max_ethereum_rpc_response_bytes: usize,
    #[arg(long, default_value_t = 4 * 1024 * 1024)]
    max_ethereum_read_bytes: usize,
    #[arg(long, default_value_t = 128)]
    max_proxy_implementations: usize,
    /// Dedicated owner of the completed Sourcify import
    #[arg(long)]
    owner: String,
    #[arg(long)]
    arkiv_chain_id: u64,
    /// Entity format: v3 payload entities (prefill) or udc (v4 UDC entities)
    #[arg(long, value_enum, default_value = "v3")]
    format: Format,
    /// v3: validated state root of block 0 containing the completed import (required)
    #[arg(long)]
    genesis_state_root: Option<String>,
    /// udc: arkiv.schema prefix of the dataset (entities are <prefix>.<kind>) [default: sourcify.v4]
    #[arg(long)]
    schema_prefix: Option<String>,
    /// udc: pinned (one block per request; a node with history), head (a head-only node such
    /// as Jalebi) or auto (decided at startup) [default: auto]
    #[arg(long, value_enum)]
    read_view: Option<ReadView>,
    /// udc: expected hash of block 0, checked at the start of every read
    #[arg(long)]
    genesis_hash: Option<String>,
    #[arg(long, default_value = "127.0.0.1:3000")]
    listen: SocketAddr,
    #[arg(long, default_value_t = 8)]
    max_concurrent_requests: usize,
    #[arg(long, default_value_t = 120)]
    request_timeout_seconds: u64,
    #[arg(long, default_value_t = 100_000)]
    max_dependency_reads: usize,
    /// Parallel Arkiv RPC calls of the whole server [default: 16 for v3, 4 for udc]
    #[arg(long)]
    max_rpc_concurrency: Option<usize>,
    #[arg(long, default_value_t = 15)]
    rpc_timeout_seconds: u64,
    #[arg(long, default_value_t = 100_000)]
    max_rpc_requests: usize,
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    max_rpc_response_bytes: usize,
    #[arg(long, default_value_t = 256 * 1024 * 1024)]
    max_read_bytes: usize,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    V3,
    Udc,
}
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum ReadView {
    Auto,
    Pinned,
    Head,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    let limits = Limits {
        max_concurrent_requests: args.max_concurrent_requests,
        request_timeout: Duration::from_secs(args.request_timeout_seconds),
        max_dependency_reads: args.max_dependency_reads,
    };
    let backend: Arc<dyn Backend> = match args.format {
        Format::V3 => {
            anyhow::ensure!(
                args.genesis_hash.is_none(),
                "--genesis-hash applies to --format udc; v3 checks --genesis-state-root"
            );
            anyhow::ensure!(
                args.schema_prefix.is_none() && args.read_view.is_none(),
                "--schema-prefix and --read-view apply to --format udc"
            );
            let root = args
                .genesis_state_root
                .context("--genesis-state-root is required with --format v3")?;
            let mut config = Config::new(args.rpc_url, args.owner, args.arkiv_chain_id, root);
            config.max_rpc_concurrency = args.max_rpc_concurrency.unwrap_or(16);
            config.rpc_timeout = Duration::from_secs(args.rpc_timeout_seconds);
            config.max_rpc_requests = args.max_rpc_requests;
            config.max_rpc_response_bytes = args.max_rpc_response_bytes;
            config.max_read_bytes = args.max_read_bytes;
            Arc::new(ArkivBackend::connect(config).await?)
        }
        Format::Udc => {
            anyhow::ensure!(
                args.genesis_state_root.is_none(),
                "--genesis-state-root applies to --format v3; udc checks --genesis-hash"
            );
            let mut config = UdcConfig::new(args.rpc_url, args.owner, args.arkiv_chain_id);
            config.api_key = std::env::var("ARKIV_RPC_API_KEY")
                .ok()
                .filter(|k| !k.is_empty());
            config.prefix = args.schema_prefix.unwrap_or_else(|| "sourcify.v4".into());
            config.view = match args.read_view.unwrap_or(ReadView::Auto) {
                ReadView::Auto => View::Auto,
                ReadView::Pinned => View::Pinned,
                ReadView::Head => View::Head,
            };
            config.genesis_hash = args.genesis_hash;
            if let Some(concurrency) = args.max_rpc_concurrency {
                config.max_rpc_concurrency = concurrency;
            }
            config.rpc_timeout = Duration::from_secs(args.rpc_timeout_seconds);
            config.max_rpc_requests = args.max_rpc_requests;
            config.max_rpc_response_bytes = args.max_rpc_response_bytes;
            config.max_read_bytes = args.max_read_bytes;
            let backend = UdcBackend::connect(config).await?;
            tracing::info!(view = backend.read_view(), "v4 (UDC) dataset");
            Arc::new(backend)
        }
    };
    let ethereum = args
        .ethereum_rpc_url
        .map(|url| {
            let mut config = EthereumConfig::new(url);
            config.max_rpc_concurrency = args.max_ethereum_rpc_concurrency;
            config.max_rpc_requests = args.max_ethereum_rpc_requests;
            config.rpc_timeout = Duration::from_secs(args.ethereum_rpc_timeout_seconds);
            config.max_rpc_response_bytes = args.max_ethereum_rpc_response_bytes;
            config.max_read_bytes = args.max_ethereum_read_bytes;
            config.max_implementations = args.max_proxy_implementations;
            EthereumRpc::new(config)
        })
        .transpose()?;
    let app = Application::with_ethereum(backend, limits, ethereum)?;
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(address = %listener.local_addr()?, mode = "arkiv", "contract reader listening");
    axum::serve(listener, app.router())
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            app.shutdown();
            tracing::info!("draining contract reads");
        })
        .await?;
    Ok(())
}
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl-C handler");
    }
}
