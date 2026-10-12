use arkiv_sourcify_server::{backends::bundle::BundleBackend, Application, Limits};
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

#[derive(Parser)]
#[command(
    version,
    about = "Fixture-backed Mainnet contract reader (not a production Arkiv backend)"
)]
struct Args {
    /// Completed, immutable processor bundle directory; mount it read-only
    #[arg(long)]
    bundle: PathBuf,
    #[arg(long, default_value = "127.0.0.1:3000")]
    listen: SocketAddr,
    #[arg(long, default_value_t = 8)]
    max_concurrent_requests: usize,
    #[arg(long, default_value_t = 120)]
    request_timeout_seconds: u64,
    /// Work limit, not a response byte-size limit
    #[arg(long, default_value_t = 100_000)]
    max_dependency_reads: usize,
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
    let backend = tokio::task::spawn_blocking(move || BundleBackend::open(args.bundle)).await??;
    let app = Application::new(Arc::new(backend), limits)?;
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(address = %listener.local_addr()?, mode = "fixture", "contract reader listening");
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
