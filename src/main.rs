use std::{
    fs,
    io::{Read, Write},
    sync::Arc,
    time::Duration,
};
use tracing::info;

use rust_imgproxy::{
    blossom::{BlossomState, CandidateFailureCache},
    cache::janitor_loop,
    config::{AppCfg, AppState, DEFAULT_BIND_ADDR},
    init_crypto_provider,
    server::{create_metrics_router, create_router},
    thumbnail::ThumbnailState,
};

#[tokio::main]
async fn main() {
    if std::env::args().nth(1).as_deref() == Some("--healthcheck") {
        std::process::exit(if healthcheck() { 0 } else { 1 });
    }
    #[cfg(target_os = "linux")]
    set_non_dumpable();
    init_crypto_provider();
    init_tracing();

    let cfg = AppCfg::from_env();

    // Create cache directories
    fs::create_dir_all(cfg.cache_dir.join("original")).expect("create original cache dir");
    fs::create_dir_all(cfg.cache_dir.join("processed")).expect("create processed cache dir");

    let bind_addr = cfg.bind_addr.clone();
    let metrics_bind_addr = cfg.metrics_bind_addr.clone();
    let state = AppState::new(cfg.clone());

    let thumbnail_state = Arc::new(ThumbnailState::new(cfg.max_ffmpeg_concurrent));

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

    // The cache janitor owns a clone; the same config still controls optional
    // management services below.
    tokio::spawn(janitor_loop(cfg.clone()));

    let app = create_router(state, thumbnail_state, blossom_state);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());

    if let Some(metrics_bind_addr) = metrics_bind_addr {
        let metrics_listener = tokio::net::TcpListener::bind(&metrics_bind_addr)
            .await
            .expect("bind metrics listener");
        let mut metrics_shutdown = shutdown_rx.clone();
        info!(addr = %metrics_bind_addr, "metrics listener enabled");
        tokio::spawn(async move {
            let _ = axum::serve(metrics_listener, create_metrics_router())
                .with_graceful_shutdown(async move {
                    let _ = metrics_shutdown.changed().await;
                })
                .await;
        });
    }

    info!(addr = bind_addr, "listening");
    let listener = tokio::net::TcpListener::bind(&bind_addr).await.unwrap();
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(());
    })
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

/// FFmpeg children parse hostile media under this process's UID. Without this a
/// compromised one could ptrace the server or read `/proc/<pid>/{mem,environ}`
/// (signing keys, metrics token). Also suppresses core dumps of those secrets.
/// Children are unaffected: `execve` resets the flag.
#[cfg(target_os = "linux")]
fn set_non_dumpable() {
    // SAFETY: prctl with plain integer arguments touches no Rust memory.
    let rc = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
    assert_eq!(rc, 0, "prctl(PR_SET_DUMPABLE, 0) failed");
}

/// `rust-imgproxy --healthcheck`: GET /health on the local listener. Lets the
/// runtime image ship without curl, an HTTP client an attacker could reuse.
fn healthcheck() -> bool {
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| DEFAULT_BIND_ADDR.into());
    let Some(port) = bind_addr
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok())
    else {
        return false;
    };
    let timeout = Duration::from_secs(2);
    let Ok(mut stream) =
        std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), timeout)
    else {
        return false;
    };
    let mut response = Vec::new();
    stream.set_read_timeout(Some(timeout)).is_ok()
        && stream
            .write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .is_ok()
        && stream.take(1024).read_to_end(&mut response).is_ok()
        && String::from_utf8_lossy(&response).split_whitespace().nth(1) == Some("200")
}
