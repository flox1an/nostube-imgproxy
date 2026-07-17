use crate::network_policy::is_allowed_untrusted_server;
use nostr_sdk::prelude::*;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Seed relays for fetching user server lists (kind 10063)
const SEED_RELAYS: &[&str] = &[
    "wss://nos.lol",
    "wss://nostr.mom",
    "wss://purplepag.es",
    "wss://relay.damus.io",
    "wss://relay.nostr.band",
    "wss://relay.snort.social",
    "wss://relay.primal.net",
    "wss://no.str.cr",
    "wss://nostr21.com",
    "wss://nostrue.com",
    "wss://purplerelay.com",
];

/// Cached outcome of an author server-list lookup.
#[derive(Clone, Debug)]
enum AuthorServerLookup {
    Servers(Vec<String>),
    Failed,
}

/// Cache entry for an author server list.
#[derive(Clone, Debug)]
struct AuthorServerCacheEntry {
    result: AuthorServerLookup,
    cached_at: Instant,
}

/// Cache entry for NIP-94 locations discovered by blob hash.
#[derive(Clone, Debug)]
struct BlobLocationCacheEntry {
    urls: Vec<String>,
    cached_at: Instant,
}

/// State for Blossom server resolution with caching.
pub struct BlossomState {
    /// Cache of author pubkey -> server list outcome.
    server_list_cache: Arc<RwLock<HashMap<PublicKey, AuthorServerCacheEntry>>>,
    /// Cache of blob hash -> NIP-94 locations.
    blob_location_cache: Arc<RwLock<HashMap<String, BlobLocationCacheEntry>>>,
    /// TTL for a successful kind-10063 lookup.
    cache_ttl: Duration,
    /// TTL for a failed kind-10063 lookup.
    failure_cache_ttl: Duration,
    /// TTL for a kind-1063 location lookup.
    discovery_cache_ttl: Duration,
    /// Timeout for one Nostr lookup.
    discovery_timeout: Duration,
    /// Nostr client for querying relays.
    client: Client,
}

impl BlossomState {
    /// Create Blossom resolution state with independent positive and failure TTLs.
    pub async fn new(
        cache_ttl: Duration,
        failure_cache_ttl: Duration,
        discovery_cache_ttl: Duration,
        discovery_timeout: Duration,
    ) -> Self {
        // Initialize Nostr client with seed relays.
        // We don't call client.connect() here to avoid persistent WebSocket connections;
        // the client connects on-demand when fetch_events_from() is called.
        let client = Client::default();

        for relay in SEED_RELAYS {
            if let Err(e) = client.add_relay(*relay).await {
                warn!("Failed to add relay {}: {:?}", relay, e);
            }
        }

        Self {
            server_list_cache: Arc::new(RwLock::new(HashMap::new())),
            blob_location_cache: Arc::new(RwLock::new(HashMap::new())),
            cache_ttl,
            failure_cache_ttl,
            discovery_cache_ttl,
            discovery_timeout,
            client,
        }
    }

    /// Parse pubkey from string (supports both npub and hex formats).
    fn parse_pubkey(pubkey_str: &str) -> Result<PublicKey, String> {
        if let Ok(pubkey) = PublicKey::from_bech32(pubkey_str) {
            return Ok(pubkey);
        }
        if let Ok(pubkey) = PublicKey::from_hex(pubkey_str) {
            return Ok(pubkey);
        }
        Err(format!("Invalid pubkey format: {}", pubkey_str))
    }

    async fn fetch_events(&self, filter: Filter) -> Result<Events, String> {
        tokio::time::timeout(
            self.discovery_timeout,
            self.client.fetch_events_from(
                SEED_RELAYS.to_vec(),
                vec![filter],
                Some(self.discovery_timeout),
            ),
        )
        .await
        .map_err(|_| "Nostr lookup timed out".to_string())?
        .map_err(|error| format!("Nostr lookup failed: {error:?}"))
    }

    /// Fetch an author's server list from kind 10063 (BUD-03).
    async fn fetch_author_servers(&self, pubkey: &PublicKey) -> Result<Vec<String>, String> {
        debug!("Fetching server list for pubkey: {}", pubkey);

        let events = self
            .fetch_events(
                Filter::new()
                    .kind(Kind::from(10063))
                    .author(*pubkey)
                    .limit(10),
            )
            .await?;

        let Some(event) = events.iter().max_by_key(|event| event.created_at) else {
            debug!("No server list events found for pubkey {}", pubkey);
            return Ok(Vec::new());
        };

        let mut servers = Vec::new();
        for tag in event.tags.clone() {
            let tag = tag.to_vec();
            if tag.len() >= 2 && tag[0] == "server" {
                servers.push(normalize_server_url(&tag[1]));
            }
        }

        info!(
            "Found {} servers for pubkey {}: {:?}",
            servers.len(),
            pubkey,
            servers
        );
        Ok(servers)
    }

    /// Get an author's kind-10063 server list with separate success and failure TTLs.
    pub async fn get_author_servers(&self, pubkey_str: &str) -> Result<Vec<String>, String> {
        let pubkey = Self::parse_pubkey(pubkey_str)?;

        {
            let cache = self.server_list_cache.read().await;
            if let Some(entry) = cache.get(&pubkey) {
                let ttl = match &entry.result {
                    AuthorServerLookup::Servers(_) => self.cache_ttl,
                    AuthorServerLookup::Failed => self.failure_cache_ttl,
                };
                if entry.cached_at.elapsed() < ttl {
                    return match &entry.result {
                        AuthorServerLookup::Servers(servers) => Ok(servers.clone()),
                        AuthorServerLookup::Failed => {
                            Err("cached author server lookup failure".to_string())
                        }
                    };
                }
            }
        }

        match self.fetch_author_servers(&pubkey).await {
            Ok(servers) => {
                self.server_list_cache.write().await.insert(
                    pubkey,
                    AuthorServerCacheEntry {
                        result: AuthorServerLookup::Servers(servers.clone()),
                        cached_at: Instant::now(),
                    },
                );
                Ok(servers)
            }
            Err(error) => {
                warn!("Failed to fetch author servers for {}: {}", pubkey, error);
                self.server_list_cache.write().await.insert(
                    pubkey,
                    AuthorServerCacheEntry {
                        result: AuthorServerLookup::Failed,
                        cached_at: Instant::now(),
                    },
                );
                Err(error)
            }
        }
    }

    /// Discover direct blob locations from NIP-94 kind-1063 events.
    pub async fn discover_blob_urls(&self, hash: &str) -> Result<Vec<String>, String> {
        let normalized_hash = hash.to_ascii_lowercase();
        {
            let cache = self.blob_location_cache.read().await;
            if let Some(entry) = cache.get(&normalized_hash) {
                if entry.cached_at.elapsed() < self.discovery_cache_ttl {
                    return Ok(entry.urls.clone());
                }
            }
        }

        let events = self
            .fetch_events(
                Filter::new()
                    .kind(Kind::from(1063))
                    .custom_tag(
                        SingleLetterTag::lowercase(Alphabet::X),
                        [normalized_hash.clone()],
                    )
                    .limit(20),
            )
            .await?;

        let mut urls = Vec::new();
        let mut seen = HashSet::new();
        for event in events {
            for tag in event.tags.clone() {
                let tag = tag.to_vec();
                if tag.len() < 2 || !matches!(tag[0].as_str(), "url" | "fallback") {
                    continue;
                }
                let url = &tag[1];
                if !is_allowed_untrusted_server(url) {
                    continue;
                }
                if seen.insert(url.clone()) {
                    urls.push(url.clone());
                }
            }
        }

        self.blob_location_cache.write().await.insert(
            normalized_hash,
            BlobLocationCacheEntry {
                urls: urls.clone(),
                cached_at: Instant::now(),
            },
        );
        Ok(urls)
    }
}

// ---------------------------------------------------------------------------
// URL utilities (shared across server.rs and thumbnail.rs)
// ---------------------------------------------------------------------------

/// Parse a Blossom filename as a SHA-256 hash with an optional extension.
pub fn parse_blossom_filename(filename: &str) -> Option<(&str, Option<&str>)> {
    let filename = filename.split('?').next().unwrap_or(filename);
    let (hash, ext) = match filename.rsplit_once('.') {
        Some((hash, ext)) if !ext.is_empty() => (hash, Some(ext)),
        _ => (filename, None),
    };
    if hash.len() == 64 && hash.chars().all(|character| character.is_ascii_hexdigit()) {
        Some((hash, ext))
    } else {
        None
    }
}

/// Extract a hash and required extension from a Blossom URL.
pub fn extract_blossom_hash(url: &str) -> Option<(&str, &str)> {
    let filename = url.rsplit('/').next()?;
    let (hash, ext) = parse_blossom_filename(filename)?;
    ext.map(|extension| (hash, extension))
}

/// Normalize server URL (add https:// if missing, remove trailing slash).
pub fn normalize_server_url(url: &str) -> String {
    let url = url.trim();
    let url = if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        format!("https://{}", url)
    };
    url.trim_end_matches('/').to_string()
}

/// Combine and deduplicate server lists in priority order:
/// xs (explicit hints, highest) → as (author servers) → fallback (lowest).
pub fn combine_server_lists(
    xs_servers: Option<&[String]>,
    as_servers: Option<&[String]>,
    fallback_servers: &[String],
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();

    let mut add_servers = |servers: &[String]| {
        for server in servers {
            let normalized = normalize_server_url(server);
            if !is_allowed_untrusted_server(&normalized) {
                warn!("Ignoring private or invalid Blossom upstream: {}", server);
                continue;
            }
            let lowercase = normalized.to_lowercase();
            if seen.insert(lowercase) {
                result.push(normalized);
            }
        }
    };

    if let Some(xs) = xs_servers {
        add_servers(xs);
    }
    if let Some(author_servers) = as_servers {
        add_servers(author_servers);
    }
    add_servers(fallback_servers);

    result
}

fn blob_url(server: &str, hash: &str, ext: Option<&str>) -> String {
    match ext {
        Some(extension) => format!("{}/{hash}.{extension}", server.trim_end_matches('/')),
        None => format!("{}/{hash}", server.trim_end_matches('/')),
    }
}

fn has_expected_hash(bytes: &[u8], expected_hash: &str) -> bool {
    hex::encode(Sha256::digest(bytes)).eq_ignore_ascii_case(expected_hash)
}

async fn fetch_candidate(
    http: &reqwest::Client,
    url: &str,
    hash: &str,
    deadline: Instant,
) -> Result<bytes::Bytes, crate::error::SvcError> {
    use crate::error::SvcError;

    if !is_allowed_untrusted_server(url) {
        return Err(SvcError::UpstreamError(400));
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(SvcError::UpstreamError(504));
    }

    tokio::time::timeout(remaining, async {
        let response = http.get(url).send().await?;
        if !response.status().is_success() {
            return Err(SvcError::UpstreamError(response.status().as_u16()));
        }
        response.bytes().await.map_err(SvcError::from)
    })
    .await
    .map_err(|_| SvcError::UpstreamError(504))?
    .and_then(|bytes| {
        if has_expected_hash(&bytes, hash) {
            Ok(bytes)
        } else {
            Err(SvcError::UpstreamError(502))
        }
    })
}

/// Fetch a hash-addressed blob through server-derived and NIP-94 direct URLs.
///
/// All candidates share one deadline and successful bytes must match `hash`.
pub async fn fetch_blob(
    http: &reqwest::Client,
    servers: &[String],
    discovered_urls: &[String],
    hash: &str,
    ext: Option<&str>,
    deadline: Instant,
) -> Result<bytes::Bytes, crate::error::SvcError> {
    use crate::error::SvcError;

    let mut last_error = SvcError::UpstreamError(404);
    let mut attempted = 0;
    let mut seen = HashSet::new();

    for server in servers {
        let url = blob_url(server, hash, ext);
        if !seen.insert(url.clone()) {
            continue;
        }
        attempted += 1;
        match fetch_candidate(http, &url, hash, deadline).await {
            Ok(bytes) => return Ok(bytes),
            Err(error) => {
                debug!("Blob candidate {} failed: {:?}", url, error);
                last_error = error;
            }
        }
    }

    for url in discovered_urls {
        if !seen.insert(url.clone()) {
            continue;
        }
        attempted += 1;
        match fetch_candidate(http, url, hash, deadline).await {
            Ok(bytes) => return Ok(bytes),
            Err(error) => {
                debug!("Discovered blob candidate {} failed: {:?}", url, error);
                last_error = error;
            }
        }
    }

    warn!("all {} blob candidates failed for {}", attempted, hash);
    Err(last_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SvcError;
    use axum::{routing::get, Router};
    use std::{net::SocketAddr, time::Duration};

    async fn spawn_blob_server(body: Vec<u8>) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app = Router::new().fallback(get(move || {
            let body = body.clone();
            async move { body }
        }));
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        address
    }

    #[test]
    fn test_normalize_server_url() {
        assert_eq!(normalize_server_url("example.com"), "https://example.com");
        assert_eq!(normalize_server_url("example.com/"), "https://example.com");
        assert_eq!(
            normalize_server_url("https://example.com"),
            "https://example.com"
        );
        assert_eq!(
            normalize_server_url("https://example.com/"),
            "https://example.com"
        );
        assert_eq!(
            normalize_server_url("http://example.com"),
            "http://example.com"
        );
    }

    #[test]
    fn test_combine_server_lists() {
        let xs = vec!["server1.com".to_string()];
        let as_s = vec!["server2.com".to_string(), "SERVER1.COM".to_string()];
        let fallback = vec!["server3.com".to_string()];

        let combined = combine_server_lists(Some(&xs), Some(&as_s), &fallback);

        assert_eq!(combined.len(), 3);
        assert_eq!(combined[0], "https://server1.com");
        assert_eq!(combined[1], "https://server2.com");
        assert_eq!(combined[2], "https://server3.com");
    }

    #[test]
    fn test_combine_server_lists_rejects_private_hints() {
        let xs = vec![
            "127.0.0.1:3000".to_string(),
            "https://cdn.example.com".to_string(),
        ];
        let author_servers = vec!["http://10.0.0.2".to_string()];

        let combined = combine_server_lists(Some(&xs), Some(&author_servers), &[]);

        assert_eq!(combined, vec!["https://cdn.example.com"]);
    }

    #[test]
    fn test_extract_blossom_hash() {
        let hash = "b".repeat(64);
        let url = format!("https://example.com/{}.mp4", hash);
        let result = extract_blossom_hash(&url);
        assert!(result.is_some());
        let (h, ext) = result.unwrap();
        assert_eq!(h, hash);
        assert_eq!(ext, "mp4");
    }

    #[test]
    fn test_extract_blossom_hash_with_query() {
        let hash = "c".repeat(64);
        let url = format!("https://example.com/{}.jpg?foo=bar", hash);
        let (h, ext) = extract_blossom_hash(&url).unwrap();
        assert_eq!(h, hash);
        assert_eq!(ext, "jpg");
    }

    #[test]
    fn test_parse_blossom_filename_supports_bare_hash() {
        let hash = "d".repeat(64);
        assert_eq!(parse_blossom_filename(&hash), Some((hash.as_str(), None)));
        assert_eq!(
            parse_blossom_filename(&format!("{hash}.webp")),
            Some((hash.as_str(), Some("webp")))
        );
    }

    #[test]
    fn test_hash_verification_rejects_mismatched_bytes() {
        let bytes = b"verified blob";
        let hash = hex::encode(Sha256::digest(bytes));
        assert!(has_expected_hash(bytes, &hash));
        assert!(!has_expected_hash(bytes, &"0".repeat(64)));
    }

    #[tokio::test]
    async fn fetch_blob_skips_corrupt_candidate() {
        let expected_bytes = b"verified blob".to_vec();
        let hash = hex::encode(Sha256::digest(&expected_bytes));
        let corrupt_server = spawn_blob_server(b"corrupt blob".to_vec()).await;
        let verified_server = spawn_blob_server(expected_bytes.clone()).await;
        let http = reqwest::Client::builder()
            .resolve("corrupt.example", corrupt_server)
            .resolve("verified.example", verified_server)
            .build()
            .unwrap();
        let servers = vec![
            format!("http://corrupt.example:{}", corrupt_server.port()),
            format!("http://verified.example:{}", verified_server.port()),
        ];

        let bytes = fetch_blob(
            &http,
            &servers,
            &[],
            &hash,
            Some("bin"),
            Instant::now() + Duration::from_secs(1),
        )
        .await
        .unwrap();

        assert_eq!(bytes.as_ref(), expected_bytes);
    }

    #[tokio::test]
    async fn fetch_blob_honors_aggregate_deadline() {
        let result = fetch_blob(
            &reqwest::Client::new(),
            &["https://cdn.example.com".to_string()],
            &[],
            &"a".repeat(64),
            Some("jpg"),
            Instant::now(),
        )
        .await;

        assert!(matches!(result, Err(SvcError::UpstreamError(504))));
    }
}
