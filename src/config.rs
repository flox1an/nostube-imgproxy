use std::{path::PathBuf, time::Duration};
use reqwest::Client;

#[derive(Clone)]
pub struct AppCfg {
    pub bind_addr: String,
    pub cache_dir: PathBuf,
    pub cache_ttl: Duration,
    pub fetch_timeout: Duration,
    pub max_image_bytes: usize,
    pub blossom_fallback_servers: Vec<String>,
}

impl AppCfg {
    pub fn from_env() -> Self {
        // Default Blossom CDN fallback servers
        let default_fallbacks = vec![
            "https://cdn.satellite.earth".to_string(),
            "https://image.nostr.build".to_string(),
            "https://nostr.download".to_string(),
            "https://cdn.hzrd149.com".to_string(),
        ];

        let blossom_fallback_servers = std::env::var("BLOSSOM_FALLBACK_SERVERS")
            .ok()
            .map(|s| s.split(',').map(|s| s.trim().to_string()).collect())
            .unwrap_or(default_fallbacks);

        Self {
            bind_addr: std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into()),
            cache_dir: PathBuf::from(std::env::var("CACHE_DIR").unwrap_or_else(|_| "cache".into())),
            cache_ttl: Duration::from_secs(
                std::env::var("CACHE_TTL_SECS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(86400),
            ),
            fetch_timeout: Duration::from_secs(
                std::env::var("FETCH_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(10),
            ),
            max_image_bytes: std::env::var("MAX_IMAGE_BYTES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(16 * 1024 * 1024),
            blossom_fallback_servers,
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub cfg: AppCfg,
    pub http: Client,
}

impl AppState {
    pub fn new(cfg: AppCfg) -> Self {
        let http = Client::builder()
            .timeout(cfg.fetch_timeout)
            .user_agent("rust-imgproxy/0.1")
            .build()
            .expect("reqwest client");

        Self { cfg, http }
    }
}

