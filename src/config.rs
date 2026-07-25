use crate::network_policy::public_dns_resolver;
use reqwest::{redirect, Client};
use std::{path::PathBuf, time::Duration};

#[derive(Clone)]
pub struct AppCfg {
    pub bind_addr: String,
    pub cache_dir: PathBuf,
    pub cache_ttl: Duration,
    pub fetch_timeout: Duration,
    pub blossom_failover_timeout: Duration,
    pub max_image_bytes: usize,
    pub blossom_fallback_servers: Vec<String>,
    pub blossom_negative_not_found_ttl: Duration,
    pub blossom_negative_permanent_ttl: Duration,
    pub blossom_negative_transient_ttl: Duration,
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
            blossom_failover_timeout: Duration::from_secs(
                std::env::var("BLOSSOM_FAILOVER_TIMEOUT_SECS")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(15),
            ),
            max_image_bytes: std::env::var("MAX_IMAGE_BYTES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(16 * 1024 * 1024),
            blossom_fallback_servers,
            blossom_negative_not_found_ttl: Duration::from_secs(
                std::env::var("BLOSSOM_NEGATIVE_CACHE_NOT_FOUND_TTL_SECS")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(900),
            ),
            blossom_negative_permanent_ttl: Duration::from_secs(
                std::env::var("BLOSSOM_NEGATIVE_CACHE_PERMANENT_TTL_SECS")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(3600),
            ),
            blossom_negative_transient_ttl: Duration::from_secs(
                std::env::var("BLOSSOM_NEGATIVE_CACHE_TRANSIENT_TTL_SECS")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(60),
            ),
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
        // `reqwest` is built with `rustls-no-provider`, so a provider must be
        // installed before any client is constructed or `build()` panics.
        crate::init_crypto_provider();

        let http = Client::builder()
            .timeout(cfg.fetch_timeout)
            .redirect(redirect::Policy::none())
            .dns_resolver(public_dns_resolver())
            .user_agent("rust-imgproxy/0.1")
            .build()
            .expect("reqwest client");

        Self { cfg, http }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex, MutexGuard};

    /// `AppCfg::from_env` reads process-global state, so the tests that mutate
    /// the environment must not run concurrently with one another.
    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn env_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Every variable `from_env` consults, cleared so each test starts clean.
    const MANAGED_VARS: &[&str] = &[
        "BIND_ADDR",
        "CACHE_DIR",
        "CACHE_TTL_SECS",
        "FETCH_TIMEOUT_SECS",
        "BLOSSOM_FAILOVER_TIMEOUT_SECS",
        "MAX_IMAGE_BYTES",
        "BLOSSOM_FALLBACK_SERVERS",
        "BLOSSOM_NEGATIVE_CACHE_NOT_FOUND_TTL_SECS",
        "BLOSSOM_NEGATIVE_CACHE_PERMANENT_TTL_SECS",
        "BLOSSOM_NEGATIVE_CACHE_TRANSIENT_TTL_SECS",
    ];

    fn clear_managed_vars() {
        for key in MANAGED_VARS {
            std::env::remove_var(key);
        }
    }

    /// Run `body` with a pristine environment, restoring the prior values after.
    fn with_env<T>(vars: &[(&str, &str)], body: impl FnOnce() -> T) -> T {
        let _guard = env_lock();
        let saved: Vec<(String, Option<String>)> = MANAGED_VARS
            .iter()
            .map(|key| (key.to_string(), std::env::var(key).ok()))
            .collect();

        clear_managed_vars();
        for (key, value) in vars {
            std::env::set_var(key, value);
        }

        let result = body();

        clear_managed_vars();
        for (key, value) in saved {
            if let Some(value) = value {
                std::env::set_var(key, value);
            }
        }
        result
    }

    #[test]
    fn from_env_uses_documented_defaults_when_nothing_is_set() {
        let cfg = with_env(&[], AppCfg::from_env);

        assert_eq!(cfg.bind_addr, "127.0.0.1:8080");
        assert_eq!(cfg.cache_dir, PathBuf::from("cache"));
        assert_eq!(cfg.cache_ttl, Duration::from_secs(86_400));
        assert_eq!(cfg.fetch_timeout, Duration::from_secs(10));
        assert_eq!(cfg.blossom_failover_timeout, Duration::from_secs(15));
        assert_eq!(cfg.max_image_bytes, 16 * 1024 * 1024);
        assert_eq!(cfg.blossom_negative_not_found_ttl, Duration::from_secs(900));
        assert_eq!(
            cfg.blossom_negative_permanent_ttl,
            Duration::from_secs(3600)
        );
        assert_eq!(cfg.blossom_negative_transient_ttl, Duration::from_secs(60));
    }

    #[test]
    fn from_env_ships_a_non_empty_default_fallback_server_list() {
        let cfg = with_env(&[], AppCfg::from_env);
        assert!(!cfg.blossom_fallback_servers.is_empty());
        for server in &cfg.blossom_fallback_servers {
            assert!(server.starts_with("https://"), "got {server:?}");
        }
    }

    #[test]
    fn from_env_honours_every_scalar_override() {
        let cfg = with_env(
            &[
                ("BIND_ADDR", "0.0.0.0:9999"),
                ("CACHE_DIR", "/var/tmp/imgcache"),
                ("CACHE_TTL_SECS", "120"),
                ("FETCH_TIMEOUT_SECS", "7"),
                ("BLOSSOM_FAILOVER_TIMEOUT_SECS", "21"),
                ("MAX_IMAGE_BYTES", "2048"),
                ("BLOSSOM_NEGATIVE_CACHE_NOT_FOUND_TTL_SECS", "11"),
                ("BLOSSOM_NEGATIVE_CACHE_PERMANENT_TTL_SECS", "22"),
                ("BLOSSOM_NEGATIVE_CACHE_TRANSIENT_TTL_SECS", "33"),
            ],
            AppCfg::from_env,
        );

        assert_eq!(cfg.bind_addr, "0.0.0.0:9999");
        assert_eq!(cfg.cache_dir, PathBuf::from("/var/tmp/imgcache"));
        assert_eq!(cfg.cache_ttl, Duration::from_secs(120));
        assert_eq!(cfg.fetch_timeout, Duration::from_secs(7));
        assert_eq!(cfg.blossom_failover_timeout, Duration::from_secs(21));
        assert_eq!(cfg.max_image_bytes, 2048);
        assert_eq!(cfg.blossom_negative_not_found_ttl, Duration::from_secs(11));
        assert_eq!(cfg.blossom_negative_permanent_ttl, Duration::from_secs(22));
        assert_eq!(cfg.blossom_negative_transient_ttl, Duration::from_secs(33));
    }

    #[test]
    fn from_env_splits_and_trims_the_fallback_server_list() {
        let cfg = with_env(
            &[(
                "BLOSSOM_FALLBACK_SERVERS",
                "https://a.example ,https://b.example,  https://c.example  ",
            )],
            AppCfg::from_env,
        );

        assert_eq!(
            cfg.blossom_fallback_servers,
            vec![
                "https://a.example".to_string(),
                "https://b.example".to_string(),
                "https://c.example".to_string(),
            ]
        );
    }

    #[test]
    fn from_env_falls_back_to_defaults_for_unparseable_numbers() {
        let cfg = with_env(
            &[
                ("CACHE_TTL_SECS", "not-a-number"),
                ("MAX_IMAGE_BYTES", ""),
                ("FETCH_TIMEOUT_SECS", "-5"),
            ],
            AppCfg::from_env,
        );

        // Malformed values must not crash startup; the defaults stand instead.
        assert_eq!(cfg.cache_ttl, Duration::from_secs(86_400));
        assert_eq!(cfg.max_image_bytes, 16 * 1024 * 1024);
        assert_eq!(cfg.fetch_timeout, Duration::from_secs(10));
    }

    #[test]
    fn app_state_builds_a_client_and_preserves_its_config() {
        let cfg = with_env(&[("BIND_ADDR", "127.0.0.1:4321")], AppCfg::from_env);
        // Also asserts that `AppState::new` installs a crypto provider rather
        // than panicking inside reqwest's builder.
        let state = AppState::new(cfg);
        assert_eq!(state.cfg.bind_addr, "127.0.0.1:4321");
    }
}
