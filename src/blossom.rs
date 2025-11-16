use nostr_sdk::prelude::*;
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

        // Initialize Nostr client with seed relays
        let client = Client::default();

        // Add all seed relays
        for relay in SEED_RELAYS {
            if let Err(e) = client.add_relay(*relay).await {
                warn!("Failed to add relay {}: {:?}", relay, e);
            }
        }

        // Connect to relays
        client.connect().await;

        Self {
            server_list_cache: Arc::new(RwLock::new(HashMap::new())),
            cache_ttl,
            client,
        }
    }

    /// Parse pubkey from string (supports both npub and hex formats)
    fn parse_pubkey(pubkey_str: &str) -> Result<PublicKey, String> {
        // Try parsing as npub (Bech32) first
        if let Ok(pubkey) = PublicKey::from_bech32(pubkey_str) {
            return Ok(pubkey);
        }

        // Try parsing as hex
        if let Ok(pubkey) = PublicKey::from_hex(pubkey_str) {
            return Ok(pubkey);
        }

        Err(format!("Invalid pubkey format: {}", pubkey_str))
    }

    /// Fetch author's server list from Nostr (kind 10063 - BUD-03)
    async fn fetch_author_servers(&self, pubkey: &PublicKey) -> Result<Vec<String>, String> {
        debug!("Fetching server list for pubkey: {}", pubkey);

        // Create filter for kind 10063 events from this author
        let filter = Filter::new()
            .kind(Kind::from(10063))
            .author(*pubkey)
            .limit(10);

        // Fetch events from relays with timeout
        let timeout = Duration::from_secs(10);

        // Use fetch_events_from to fetch events from specific relays
        let events = match tokio::time::timeout(
            timeout,
            self.client.fetch_events_from(SEED_RELAYS.to_vec(), vec![filter], Some(timeout))
        ).await {
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

        // Get the most recent event
        let event = events.iter().max_by_key(|e| e.created_at).unwrap();

        debug!("Found server list event: {} with {} tags", event.id, event.tags.len());

        // Extract server URLs from "server" tags
        let mut servers = Vec::new();
        for tag in event.tags.clone() {
            let tag_vec = tag.to_vec();
            if tag_vec.len() >= 2 && tag_vec[0] == "server" {
                let server_url = normalize_server_url(&tag_vec[1]);
                servers.push(server_url);
            }
        }

        info!("Found {} servers for pubkey {}: {:?}", servers.len(), pubkey, servers);

        Ok(servers)
    }

    /// Get author's server list (with caching)
    pub async fn get_author_servers(&self, pubkey_str: &str) -> Result<Vec<String>, String> {
        let pubkey = Self::parse_pubkey(pubkey_str)?;

        // Check cache first
        {
            let cache = self.server_list_cache.read().await;
            if let Some(entry) = cache.get(&pubkey) {
                // Check if cache is still valid
                if entry.cached_at.elapsed() < self.cache_ttl {
                    debug!("Cache hit for pubkey {}", pubkey);
                    return Ok(entry.servers.clone());
                } else {
                    debug!("Cache expired for pubkey {}", pubkey);
                }
            }
        }

        // Cache miss or expired - fetch from Nostr
        debug!("Cache miss for pubkey {}, fetching from Nostr", pubkey);
        let servers = self.fetch_author_servers(&pubkey).await?;

        // Update cache
        {
            let mut cache = self.server_list_cache.write().await;
            cache.insert(pubkey, CacheEntry {
                servers: servers.clone(),
                cached_at: Instant::now(),
            });
        }

        Ok(servers)
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

/// Combine and deduplicate server lists in priority order
/// Priority: xs (highest) -> as -> fallback (lowest)
pub fn combine_server_lists(
    xs_servers: Option<&[String]>,
    as_servers: Option<&[String]>,
    fallback_servers: &[String],
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();

    // Helper to add servers with deduplication
    let mut add_servers = |servers: &[String]| {
        for server in servers {
            let normalized = normalize_server_url(server);
            let lowercase = normalized.to_lowercase();
            if !seen.contains(&lowercase) {
                seen.insert(lowercase);
                result.push(normalized);
            }
        }
    };

    // Add in priority order
    if let Some(xs) = xs_servers {
        add_servers(xs);
    }
    if let Some(as_s) = as_servers {
        add_servers(as_s);
    }
    add_servers(fallback_servers);

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_server_url() {
        assert_eq!(normalize_server_url("example.com"), "https://example.com");
        assert_eq!(normalize_server_url("example.com/"), "https://example.com");
        assert_eq!(normalize_server_url("https://example.com"), "https://example.com");
        assert_eq!(normalize_server_url("https://example.com/"), "https://example.com");
        assert_eq!(normalize_server_url("http://example.com"), "http://example.com");
    }

    #[test]
    fn test_combine_server_lists() {
        let xs = vec!["server1.com".to_string()];
        let as_s = vec!["server2.com".to_string(), "SERVER1.COM".to_string()];
        let fallback = vec!["server3.com".to_string()];

        let combined = combine_server_lists(Some(&xs), Some(&as_s), &fallback);

        // Should deduplicate SERVER1.COM and preserve order
        assert_eq!(combined.len(), 3);
        assert_eq!(combined[0], "https://server1.com");
        assert_eq!(combined[1], "https://server2.com");
        assert_eq!(combined[2], "https://server3.com");
    }
}
