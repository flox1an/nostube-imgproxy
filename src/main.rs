use std::{fs, sync::Arc};
use tracing::info;

mod blossom;
mod cache;
mod config;
mod error;
mod server;
mod thumbnail;
mod transform;

use blossom::BlossomState;
use cache::janitor_loop;
use config::{AppCfg, AppState};
use server::create_router;
use thumbnail::ThumbnailState;

#[tokio::main]
async fn main() {
    init_tracing();

    let cfg = AppCfg::from_env();
    
    // Create cache directories
    fs::create_dir_all(cfg.cache_dir.join("original")).expect("create original cache dir");
    fs::create_dir_all(cfg.cache_dir.join("processed")).expect("create processed cache dir");

    let bind_addr = cfg.bind_addr.clone();
    let state = AppState::new(cfg.clone());

    // Create thumbnail state with max concurrent ffmpeg processes
    let max_ffmpeg_concurrent = std::env::var("MAX_FFMPEG_CONCURRENT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let thumbnail_state = Arc::new(ThumbnailState::new(max_ffmpeg_concurrent));

    // Create blossom state with configurable cache TTL
    let blossom_cache_ttl_hours = std::env::var("BLOSSOM_SERVER_LIST_CACHE_TTL_HOURS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24);
    let blossom_state = Arc::new(BlossomState::new(blossom_cache_ttl_hours).await);

    // Spawn janitor
    tokio::spawn(async move { janitor_loop(cfg).await });

    let app = create_router(state, thumbnail_state, blossom_state);

    info!(addr = bind_addr, "listening");
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .unwrap();
    
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
    
    info!("server shutdown complete");
}

async fn shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("received Ctrl+C signal");
        },
        _ = terminate => {
            info!("received terminate signal");
        },
    }
}

fn init_tracing() {
    let env_filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into());
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .init();
}
