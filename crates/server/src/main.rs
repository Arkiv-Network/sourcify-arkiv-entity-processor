use arkiv_sourcify_server::{
    backends::arkiv::{ArkivBackend, Config},
    Application, Limits,
};
use clap::Parser;
use std::{net::SocketAddr, sync::Arc, time::Duration};

#[derive(Parser)]
#[command(version, about = "Arkiv-backed Sourcify Mainnet contract reader")]
struct Args {
    #[arg(long)]
    rpc_url: String,
    /// Dedicated owner of the completed Sourcify import
    #[arg(long)]
    owner: String,
    #[arg(long)]
    arkiv_chain_id: u64,
    /// Validated state root of block 0 containing the completed import
    #[arg(long)]
    genesis_state_root: String,
    #[arg(long, default_value = "127.0.0.1:3000")]
    listen: SocketAddr,
    #[arg(long, default_value_t = 8)]
    max_concurrent_requests: usize,
    #[arg(long, default_value_t = 120)]
    request_timeout_seconds: u64,
    #[arg(long, default_value_t = 100_000)]
    max_dependency_reads: usize,
    #[arg(long, default_value_t = 16)]
    max_rpc_concurrency: usize,
    #[arg(long, default_value_t = 15)]
    rpc_timeout_seconds: u64,
    #[arg(long, default_value_t = 100_000)]
    max_rpc_requests: usize,
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    max_rpc_response_bytes: usize,
    #[arg(long, default_value_t = 256 * 1024 * 1024)]
    max_read_bytes: usize,
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
    let mut config = Config::new(
        args.rpc_url,
        args.owner,
        args.arkiv_chain_id,
        args.genesis_state_root,
    );
    config.max_rpc_concurrency = args.max_rpc_concurrency;
    config.rpc_timeout = Duration::from_secs(args.rpc_timeout_seconds);
    config.max_rpc_requests = args.max_rpc_requests;
    config.max_rpc_response_bytes = args.max_rpc_response_bytes;
    config.max_read_bytes = args.max_read_bytes;
    let backend = ArkivBackend::connect(config).await?;
    let app = Application::new(Arc::new(backend), limits)?;
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
