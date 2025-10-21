use std::fs;
use tracing::info;

mod cache;
mod config;
mod error;
mod server;
mod transform;

use cache::janitor_loop;
use config::{AppCfg, AppState};
use server::create_router;

#[tokio::main]
async fn main() {
    init_tracing();

    let cfg = AppCfg::from_env();
    
    // Create cache directories
    fs::create_dir_all(cfg.cache_dir.join("original")).expect("create original cache dir");
    fs::create_dir_all(cfg.cache_dir.join("processed")).expect("create processed cache dir");

    let bind_addr = cfg.bind_addr.clone();
    let state = AppState::new(cfg.clone());

    // Spawn janitor
    tokio::spawn(async move { janitor_loop(cfg).await });

    let app = create_router(state);

    info!(addr = bind_addr, "listening");
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .unwrap();
    axum::serve(listener, app).await.unwrap();
}

fn init_tracing() {
    let env_filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into());
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .init();
}
