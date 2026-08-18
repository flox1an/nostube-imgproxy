use crate::{
    network_policy::{guarded_redirect_policy, public_dns_resolver},
    signing::UrlSigningKeys,
};

use reqwest::Client;
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
    /// Strict per-axis cap applied before a decoder allocates its framebuffer.
    pub max_image_dimension: u32,
    /// Ceiling on the bytes a single decode may allocate. `max_image_bytes`
    /// only bounds the *compressed* payload, so this is what actually stops a
    /// decompression bomb.
    pub max_decode_alloc_bytes: u64,
    /// Simultaneous decode/resize/encode jobs. Sized to the CPU, not to the
    /// request rate: image work is what saturates a small edge node.
    pub cpu_concurrency: usize,
    /// Global in-flight HTTP request ceiling.
    pub max_inflight_requests: usize,
    /// Wall-clock budget for one HTTP request, enforced by the router.
    pub request_timeout: Duration,
    /// Wall-clock budget for one FFmpeg invocation.
    pub ffmpeg_timeout: Duration,
    /// Ceiling on the total bytes both cache directories may occupy. The TTL
    /// alone cannot bound the cache: an attacker requesting distinct URLs fills
    /// the disk long before the oldest entry expires.
    pub max_cache_bytes: u64,
    /// Total remote bytes the local media proxy may relay while producing one
    /// thumbnail. This bounds work without imposing a full-video-size cap, so
    /// seekable multi-gigabyte sources remain usable.
    pub max_video_probe_bytes: u64,
    /// Total Blossom candidates tried for one blob, across request hints,
    /// author servers, fallbacks and NIP-94 discovery. Bounds the fan-out a
    /// single request can aim at third-party hosts.
    pub max_blob_candidates: usize,
    /// How many `xs=` request hints are honoured before the rest are dropped.
    pub max_server_hints: usize,
    /// Requests allowed to queue for a CPU permit before the node sheds load.
    /// Each waiter pins the already-downloaded original in memory, so an
    /// unbounded queue is an unbounded heap.
    pub cpu_queue_depth: usize,
    /// Separate listener for `/metrics`. `None` keeps it off the public router
    /// entirely; operators opt in with a management-network address.
    pub metrics_bind_addr: Option<String>,
    /// Simultaneous FFmpeg processes allowed for video thumbnail extraction.
    pub max_ffmpeg_concurrent: usize,
    /// Versioned HMAC keys accepted by `/v1/{key_id}/{signature}/...`.
    /// Secrets remain opaque and are never formatted into logs.
    pub url_signing_keys: UrlSigningKeys,
    /// Temporary migration switch for the legacy unsigned `/insecure` and
    /// `/thumb` routes. Turn this off after every caller emits signed v1 URLs.
    pub allow_unsigned_urls: bool,
    /// Require `exp=<unix-seconds>` in every signed URL. This is on by default
    /// so leaked capability URLs have a bounded lifetime.
    pub require_signed_url_expiry: bool,
    /// Registers the unsigned preset-bounded thumbnail route
    /// (`GET /v1/preset/{preset}/{filename}`). Safe by construction — no
    /// secret to misconfigure, output shape is fixed to a small published
    /// preset set — so it is enabled by default.
    pub preset_thumbnails_enabled: bool,
    /// General per-IP request budget across every image/thumb route, hit or
    /// miss. Generous: a cache hit is cheap to serve.
    pub rate_ip_requests_per_min: u32,
    /// Per-IP budget for cache-miss image decode/resize/encode work.
    pub rate_ip_image_generations_per_min: u32,
    /// Per-IP budget for cache-miss video-thumbnail FFmpeg work — the most
    /// expensive path per request, budgeted far below image generation.
    pub rate_ip_video_generations_per_min: u32,
}

/// Read a `usize`/`u32`/`u64` style setting, falling back on absent or
/// unparseable input so a typo degrades to the default instead of a panic.
fn env_parsed<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_secs(key: &str, default: u64) -> Duration {
    Duration::from_secs(env_parsed(key, default))
}

fn default_cpu_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
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

        let url_signing_keys = std::env::var("URL_SIGNING_KEYS")
            .ok()
            .map(|value| {
                UrlSigningKeys::parse(&value)
                    .unwrap_or_else(|error| panic!("invalid URL_SIGNING_KEYS: {error}"))
            })
            .unwrap_or_default();
        let allow_unsigned_urls = env_parsed("ALLOW_UNSIGNED_URLS", true);
        let require_signed_url_expiry = env_parsed("REQUIRE_SIGNED_URL_EXPIRY", true);
        if url_signing_keys.is_empty() && !allow_unsigned_urls {
            panic!("URL_SIGNING_KEYS must be configured when ALLOW_UNSIGNED_URLS=false");
        }

        Self {
            bind_addr: std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into()),
            cache_dir: PathBuf::from(std::env::var("CACHE_DIR").unwrap_or_else(|_| "cache".into())),
            cache_ttl: env_secs("CACHE_TTL_SECS", 86400),
            fetch_timeout: env_secs("FETCH_TIMEOUT_SECS", 10),
            blossom_failover_timeout: env_secs("BLOSSOM_FAILOVER_TIMEOUT_SECS", 15),
            max_image_bytes: env_parsed("MAX_IMAGE_BYTES", 16 * 1024 * 1024),
            blossom_fallback_servers,
            blossom_negative_not_found_ttl: env_secs(
                "BLOSSOM_NEGATIVE_CACHE_NOT_FOUND_TTL_SECS",
                900,
            ),
            blossom_negative_permanent_ttl: env_secs(
                "BLOSSOM_NEGATIVE_CACHE_PERMANENT_TTL_SECS",
                3600,
            ),
            blossom_negative_transient_ttl: env_secs(
                "BLOSSOM_NEGATIVE_CACHE_TRANSIENT_TTL_SECS",
                60,
            ),
            max_image_dimension: env_parsed("MAX_IMAGE_DIMENSION", 16_384),
            max_decode_alloc_bytes: env_parsed("MAX_DECODE_ALLOC_BYTES", 256 * 1024 * 1024),
            cpu_concurrency: env_parsed("MAX_CPU_CONCURRENT", default_cpu_concurrency()).max(1),
            max_inflight_requests: env_parsed("MAX_INFLIGHT_REQUESTS", 256usize).max(1),
            request_timeout: env_secs("REQUEST_TIMEOUT_SECS", 30),
            ffmpeg_timeout: env_secs("FFMPEG_TIMEOUT_SECS", 20),
            max_cache_bytes: env_parsed("MAX_CACHE_BYTES", 8 * 1024 * 1024 * 1024),
            max_video_probe_bytes: env_parsed("MAX_VIDEO_PROBE_BYTES", 64 * 1024 * 1024),
            max_blob_candidates: env_parsed("MAX_BLOB_CANDIDATES", 8usize).max(1),
            max_server_hints: env_parsed("MAX_SERVER_HINTS", 4usize),
            cpu_queue_depth: env_parsed("MAX_CPU_QUEUE", 64usize).max(1),
            metrics_bind_addr: std::env::var("METRICS_BIND_ADDR").ok(),
            max_ffmpeg_concurrent: env_parsed("MAX_FFMPEG_CONCURRENT", 8usize).max(1),
            url_signing_keys,
            allow_unsigned_urls,
            require_signed_url_expiry,
            preset_thumbnails_enabled: env_parsed("PRESET_THUMBNAILS_ENABLED", true),
            rate_ip_requests_per_min: env_parsed("RATE_IP_REQUESTS_PER_MIN", 600u32).max(1),
            rate_ip_image_generations_per_min: env_parsed(
                "RATE_IP_IMAGE_GENERATIONS_PER_MIN",
                30u32,
            )
            .max(1),
            rate_ip_video_generations_per_min: env_parsed(
                "RATE_IP_VIDEO_GENERATIONS_PER_MIN",
                5u32,
            )
            .max(1),
        }
    }

    /// Decoder resource limits derived from this config.
    ///
    /// The width/height caps are strict in the `image` crate; `max_alloc` is
    /// best-effort but is the one that actually bounds a hostile image whose
    /// dimensions are legal but whose framebuffer is not.
    pub fn decode_limits(&self) -> image::Limits {
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(self.max_image_dimension);
        limits.max_image_height = Some(self.max_image_dimension);
        limits.max_alloc = Some(self.max_decode_alloc_bytes);
        limits
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
            .redirect(guarded_redirect_policy())
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
    use parking_lot::{Mutex, MutexGuard};
    use std::sync::LazyLock;

    /// `AppCfg::from_env` reads process-global state, so the tests that mutate
    /// the environment must not run concurrently with one another.
    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn env_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock()
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
        "MAX_IMAGE_DIMENSION",
        "MAX_DECODE_ALLOC_BYTES",
        "MAX_CPU_CONCURRENT",
        "MAX_INFLIGHT_REQUESTS",
        "REQUEST_TIMEOUT_SECS",
        "FFMPEG_TIMEOUT_SECS",
        "MAX_CACHE_BYTES",
        "MAX_VIDEO_PROBE_BYTES",
        "MAX_BLOB_CANDIDATES",
        "MAX_SERVER_HINTS",
        "MAX_CPU_QUEUE",
        "METRICS_BIND_ADDR",
        "MAX_FFMPEG_CONCURRENT",
        "URL_SIGNING_KEYS",
        "ALLOW_UNSIGNED_URLS",
        "REQUIRE_SIGNED_URL_EXPIRY",
        "PRESET_THUMBNAILS_ENABLED",
        "SIGNED_URL_TTL_SECS",
        "RATE_IP_REQUESTS_PER_MIN",
        "RATE_IP_IMAGE_GENERATIONS_PER_MIN",
        "RATE_IP_VIDEO_GENERATIONS_PER_MIN",
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
        assert_eq!(cfg.max_image_dimension, 16_384);
        assert_eq!(cfg.max_decode_alloc_bytes, 256 * 1024 * 1024);
        assert_eq!(cfg.max_inflight_requests, 256);
        assert_eq!(cfg.request_timeout, Duration::from_secs(30));
        assert_eq!(cfg.ffmpeg_timeout, Duration::from_secs(20));
        assert_eq!(cfg.max_cache_bytes, 8 * 1024 * 1024 * 1024);
        assert_eq!(cfg.max_video_probe_bytes, 64 * 1024 * 1024);
        assert_eq!(cfg.max_blob_candidates, 8);
        assert_eq!(cfg.max_server_hints, 4);
        assert_eq!(cfg.cpu_queue_depth, 64);
        assert_eq!(cfg.metrics_bind_addr, None);
        assert_eq!(cfg.max_ffmpeg_concurrent, 8);
        assert!(cfg.url_signing_keys.is_empty());
        assert!(cfg.allow_unsigned_urls);
        assert!(cfg.require_signed_url_expiry);
        assert!(cfg.preset_thumbnails_enabled);
        assert_eq!(cfg.rate_ip_requests_per_min, 600);
        assert_eq!(cfg.rate_ip_image_generations_per_min, 30);
        assert_eq!(cfg.rate_ip_video_generations_per_min, 5);
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
                ("MAX_IMAGE_DIMENSION", "512"),
                ("MAX_DECODE_ALLOC_BYTES", "4096"),
                ("MAX_CPU_CONCURRENT", "2"),
                ("MAX_INFLIGHT_REQUESTS", "42"),
                ("REQUEST_TIMEOUT_SECS", "17"),
                ("FFMPEG_TIMEOUT_SECS", "9"),
                ("MAX_CACHE_BYTES", "4096"),
                ("MAX_VIDEO_PROBE_BYTES", "8192"),
                ("MAX_BLOB_CANDIDATES", "3"),
                ("MAX_SERVER_HINTS", "1"),
                ("MAX_CPU_QUEUE", "5"),
                ("METRICS_BIND_ADDR", "127.0.0.1:9100"),
                (
                    "URL_SIGNING_KEYS",
                    "nostube-2026-08:AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8",
                ),
                ("ALLOW_UNSIGNED_URLS", "false"),
                ("REQUIRE_SIGNED_URL_EXPIRY", "false"),
                ("PRESET_THUMBNAILS_ENABLED", "false"),
                ("RATE_IP_REQUESTS_PER_MIN", "50"),
                ("RATE_IP_IMAGE_GENERATIONS_PER_MIN", "10"),
                ("RATE_IP_VIDEO_GENERATIONS_PER_MIN", "2"),
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
        assert_eq!(cfg.max_image_dimension, 512);
        assert_eq!(cfg.max_decode_alloc_bytes, 4096);
        assert_eq!(cfg.cpu_concurrency, 2);
        assert_eq!(cfg.max_inflight_requests, 42);
        assert_eq!(cfg.request_timeout, Duration::from_secs(17));
        assert_eq!(cfg.ffmpeg_timeout, Duration::from_secs(9));
        assert_eq!(cfg.max_cache_bytes, 4096);
        assert_eq!(cfg.max_video_probe_bytes, 8192);
        assert_eq!(cfg.max_blob_candidates, 3);
        assert_eq!(cfg.max_server_hints, 1);
        assert_eq!(cfg.cpu_queue_depth, 5);
        assert_eq!(cfg.metrics_bind_addr, Some("127.0.0.1:9100".to_string()));
        assert!(!cfg.url_signing_keys.is_empty());
        assert!(!cfg.allow_unsigned_urls);
        assert!(!cfg.require_signed_url_expiry);
        assert!(!cfg.preset_thumbnails_enabled);
        assert_eq!(cfg.rate_ip_requests_per_min, 50);
        assert_eq!(cfg.rate_ip_image_generations_per_min, 10);
        assert_eq!(cfg.rate_ip_video_generations_per_min, 2);
    }

    #[test]
    fn from_env_rejects_disabling_legacy_routes_without_a_signing_key() {
        with_env(&[("ALLOW_UNSIGNED_URLS", "false")], || {
            assert!(std::panic::catch_unwind(AppCfg::from_env).is_err());
        });
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
