use anyhow::{Context, Result};
use aquatrace_backend::{Config, StartupOptions, app_state_with_options, build_router};
use clap::Parser;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, Parser)]
#[command(version, about = "Analyze GPX routes and locate nearby drinking water")]
struct Cli {
    /// Download and import a fresh OpenStreetMap PBF extract on startup
    #[arg(long)]
    force_osm_download: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "aquatrace_backend=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();
    let config = Config::from_env()?;
    let bind_addr = config.bind_addr;
    let state = app_state_with_options(
        config,
        StartupOptions {
            force_osm_download: cli.force_osm_download,
        },
    )
    .await?;
    let app = build_router(state);

    info!("backend listening on {}", bind_addr);
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .context("failed to bind backend listener")?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("backend server failed")?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
