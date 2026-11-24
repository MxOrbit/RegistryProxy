use std::process::ExitCode;

use registry_proxy::{AppState, Config, config::Mode, router};
use tokio::net::TcpListener;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            error!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let port = config.port;
    let mode = config.mode;
    let auth = config.auth.as_str();
    let upstream = config.upstream.clone();
    let cache_dir = config.cache.as_ref().map(|cache| cache.dir.clone());

    let state = match AppState::new(config) {
        Ok(state) => state,
        Err(err) => {
            error!("{err}");
            return ExitCode::FAILURE;
        }
    };

    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(listener) => listener,
        Err(err) => {
            error!("failed to listen on port {port}: {err}");
            return ExitCode::FAILURE;
        }
    };

    info!("Registry Proxy listening on port {port}");
    info!("Mode: {}", mode.as_str());
    info!("Auth: {auth}");
    match mode {
        Mode::Single => info!("Upstream: {upstream}"),
        Mode::Route => info!("Route mode: routing by subdomain prefix"),
    }
    if let Some(dir) = cache_dir {
        info!("Cache: {}", dir.display());
    }

    if let Err(err) = axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        error!("server error: {err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("shutting down");
}
