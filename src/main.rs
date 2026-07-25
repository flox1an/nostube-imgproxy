use std::{fs, sync::Arc, time::Duration};
use tracing::info;

use rust_imgproxy::{
    blossom::{BlossomState, CandidateFailureCache},
    cache::janitor_loop,
    config::{AppCfg, AppState},
    init_crypto_provider,
    server::create_router,
    thumbnail::ThumbnailState,
};

#[tokio::main]
async fn main() {
    init_crypto_provider();
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

    // Keep transient relay failures short-lived while retaining successful lists.
    let blossom_cache_ttl_hours = std::env::var("BLOSSOM_SERVER_LIST_CACHE_TTL_HOURS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(24);
    let blossom_failure_cache_ttl = Duration::from_secs(
        std::env::var("BLOSSOM_SERVER_LIST_FAILURE_CACHE_TTL_SECS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(300),
    );
    let blossom_discovery_cache_ttl = Duration::from_secs(
        std::env::var("BLOSSOM_DISCOVERY_CACHE_TTL_SECS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(3600),
    );
    let blossom_discovery_timeout = Duration::from_secs(
        std::env::var("BLOSSOM_DISCOVERY_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(3),
    );
    let candidate_failure_cache = CandidateFailureCache::new(
        cfg.blossom_negative_not_found_ttl,
        cfg.blossom_negative_permanent_ttl,
        cfg.blossom_negative_transient_ttl,
    );
    let blossom_state = Arc::new(
        BlossomState::new(
            Duration::from_secs(blossom_cache_ttl_hours * 3600),
            blossom_failure_cache_ttl,
            blossom_discovery_cache_ttl,
            blossom_discovery_timeout,
            candidate_failure_cache,
        )
        .await,
    );

    // Spawn janitor
    tokio::spawn(async move { janitor_loop(cfg).await });

    let app = create_router(state, thumbnail_state, blossom_state);

    info!(addr = bind_addr, "listening");
    let listener = tokio::net::TcpListener::bind(&bind_addr).await.unwrap();

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
    tracing_subscriber::fmt().with_env_filter(env_filter).init();
}
