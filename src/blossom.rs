use crate::network_policy::is_allowed_untrusted_server;
use nostr_sdk::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Servers that re-encode uploaded content, so the SHA256 in the URL does not
/// correspond to the blob served. Do NOT use these as blossom fallbacks.
pub const NON_BLOSSOM_SERVERS: &[&str] = &["video.nostr.build", "cdn.nostrcheck.me"];

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

/// Cache entry for author's server list
#[derive(Clone, Debug)]
struct CacheEntry {
    servers: Vec<String>,
    cached_at: Instant,
}

/// State for Blossom server resolution with caching
pub struct BlossomState {
    /// Cache of author pubkey -> server list
    server_list_cache: Arc<RwLock<HashMap<PublicKey, CacheEntry>>>,
    /// Cache TTL duration (default: 24 hours)
    cache_ttl: Duration,
    /// Nostr client for querying relays
    client: Client,
}

impl BlossomState {
    /// Create new BlossomState with configurable cache TTL
    pub async fn new(cache_ttl_hours: u64) -> Self {
        let cache_ttl = Duration::from_secs(cache_ttl_hours * 3600);

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
            cache_ttl,
            client,
        }
    }

    /// Parse pubkey from string (supports both npub and hex formats)
    fn parse_pubkey(pubkey_str: &str) -> Result<PublicKey, String> {
        if let Ok(pubkey) = PublicKey::from_bech32(pubkey_str) {
            return Ok(pubkey);
        }
        if let Ok(pubkey) = PublicKey::from_hex(pubkey_str) {
            return Ok(pubkey);
        }
        Err(format!("Invalid pubkey format: {}", pubkey_str))
    }

    /// Fetch author's server list from Nostr (kind 10063 - BUD-03)
    async fn fetch_author_servers(&self, pubkey: &PublicKey) -> Result<Vec<String>, String> {
        debug!("Fetching server list for pubkey: {}", pubkey);

        let filter = Filter::new()
            .kind(Kind::from(10063))
            .author(*pubkey)
            .limit(10);

        let timeout = Duration::from_secs(10);

        let events = match tokio::time::timeout(
            timeout,
            self.client
                .fetch_events_from(SEED_RELAYS.to_vec(), vec![filter], Some(timeout)),
        )
        .await
        {
            Ok(Ok(events)) => events,
            Ok(Err(e)) => {
                warn!("Failed to fetch events from Nostr: {:?}", e);
                return Ok(Vec::new());
            }
            Err(_) => {
                warn!("Timeout fetching events from Nostr");
                return Ok(Vec::new());
            }
        };

        if events.is_empty() {
            debug!("No server list events found for pubkey {}", pubkey);
            return Ok(Vec::new());
        }

        let event = events.iter().max_by_key(|e| e.created_at).unwrap();
        debug!(
            "Found server list event: {} with {} tags",
            event.id,
            event.tags.len()
        );

        let mut servers = Vec::new();
        for tag in event.tags.clone() {
            let tag_vec = tag.to_vec();
            if tag_vec.len() >= 2 && tag_vec[0] == "server" {
                let server_url = normalize_server_url(&tag_vec[1]);
                servers.push(server_url);
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

    /// Get author's server list (with caching)
    pub async fn get_author_servers(&self, pubkey_str: &str) -> Result<Vec<String>, String> {
        let pubkey = Self::parse_pubkey(pubkey_str)?;

        {
            let cache = self.server_list_cache.read().await;
            if let Some(entry) = cache.get(&pubkey) {
                if entry.cached_at.elapsed() < self.cache_ttl {
                    debug!("Cache hit for pubkey {}", pubkey);
                    return Ok(entry.servers.clone());
                }
                debug!("Cache expired for pubkey {}", pubkey);
            }
        }

        debug!("Cache miss for pubkey {}, fetching from Nostr", pubkey);
        let servers = self.fetch_author_servers(&pubkey).await?;

        {
            let mut cache = self.server_list_cache.write().await;
            cache.insert(
                pubkey,
                CacheEntry {
                    servers: servers.clone(),
                    cached_at: Instant::now(),
                },
            );
        }

        Ok(servers)
    }
}

// ---------------------------------------------------------------------------
// URL utilities (shared across server.rs and thumbnail.rs)
// ---------------------------------------------------------------------------

/// Return true if `url` has the Blossom SHA-256 filename format **and** is not
/// from a server that re-encodes content (whose hash therefore won't match).
pub fn is_blossom_url(url: &str) -> bool {
    // Extract hostname via string splitting — no url crate dep needed
    let after_scheme = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    // authority is everything before the first '/'
    let authority = after_scheme.split('/').next().unwrap_or("");
    // strip userinfo if present (user@host)
    let host = authority
        .split('@')
        .next_back()
        .unwrap_or(authority)
        .to_lowercase();
    // Reject known re-encoding servers whose SHA-256 won't match the blob
    if NON_BLOSSOM_SERVERS
        .iter()
        .any(|s| host == *s || host.ends_with(&format!(".{}", s)))
    {
        return false;
    }
    extract_blossom_hash(url).is_some()
}

/// Extract `(hash, ext)` from a Blossom URL path segment (`<sha256>.<ext>`).
/// Returns `None` for non-blossom URLs or re-encoding servers.
pub fn extract_blossom_hash(url: &str) -> Option<(&str, &str)> {
    let filename = url.rsplit('/').next()?;
    // Strip query string if any
    let filename = filename.split('?').next().unwrap_or(filename);
    let (hash_part, ext) = filename.rsplit_once('.')?;
    if hash_part.len() == 64 && hash_part.chars().all(|c| c.is_ascii_hexdigit()) {
        Some((hash_part, ext))
    } else {
        None
    }
}

/// Normalize server URL (add https:// if missing, remove trailing slash)
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
/// xs (explicit hints, highest) → as (author servers) → fallback (lowest)
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
            if !seen.contains(&lowercase) {
                seen.insert(lowercase);
                result.push(normalized);
            }
        }
    };

    if let Some(xs) = xs_servers {
        add_servers(xs);
    }
    if let Some(as_s) = as_servers {
        add_servers(as_s);
    }
    add_servers(fallback_servers);

    result
}

/// Fetch a blob by trying each server in order.
///
/// Constructs `{server}/{hash}.{ext}` for each entry and returns on the first
/// successful 2xx response.  `ext` must not be empty.
pub async fn fetch_from_servers(
    http: &reqwest::Client,
    servers: &[String],
    hash: &str,
    ext: &str,
) -> Result<bytes::Bytes, crate::error::SvcError> {
    use crate::error::SvcError;

    if servers.is_empty() {
        return Err(SvcError::UpstreamError(404));
    }

    let mut last_err = SvcError::UpstreamError(404);

    for (idx, server) in servers.iter().enumerate() {
        if !is_allowed_untrusted_server(server) {
            tracing::warn!("Skipping private or invalid Blossom upstream: {}", server);
            continue;
        }
        let url = format!("{}/{}.{}", server.trim_end_matches('/'), hash, ext);
        tracing::debug!("server {}/{}: {}", idx + 1, servers.len(), url);

        match http.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                Ok(b) => {
                    tracing::info!(
                        "✓ server {}/{} ({}) → {} bytes",
                        idx + 1,
                        servers.len(),
                        server,
                        b.len()
                    );
                    return Ok(b);
                }
                Err(e) => {
                    tracing::debug!(
                        "✗ server {}/{} body read failed: {:?}",
                        idx + 1,
                        servers.len(),
                        e
                    );
                    last_err = SvcError::UpstreamError(500);
                }
            },
            Ok(resp) => {
                let status = resp.status().as_u16();
                tracing::debug!("✗ server {}/{} returned {}", idx + 1, servers.len(), status);
                last_err = SvcError::UpstreamError(status);
            }
            Err(e) => {
                tracing::debug!(
                    "✗ server {}/{} request error: {:?}",
                    idx + 1,
                    servers.len(),
                    e
                );
                last_err = SvcError::UpstreamError(500);
            }
        }
    }

    tracing::warn!("all {} servers failed for {}.{}", servers.len(), hash, ext);
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_is_blossom_url_rejects_non_blossom_servers() {
        let hash = "a".repeat(64);
        assert!(!is_blossom_url(&format!(
            "https://video.nostr.build/{}.mp4",
            hash
        )));
        assert!(!is_blossom_url(&format!(
            "https://cdn.nostrcheck.me/{}.mp4",
            hash
        )));
        assert!(is_blossom_url(&format!(
            "https://cdn.satellite.earth/{}.mp4",
            hash
        )));
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
}
