use crate::network_policy::is_allowed_untrusted_server;
use nostr_sdk::prelude::*;
use sha2::{Digest, Sha256};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, warn};

/// NIP-01 replaceable event kind carrying a BUD-03 Blossom server list.
const KIND_SERVER_LIST: u16 = 10063;
/// NIP-94 file-metadata event kind carrying `x` (blob hash) and `url` tags.
const KIND_FILE_METADATA: u16 = 1063;
/// How many of the seed relays a single lookup actually queries (see
/// `fetch_events` for the rationale behind the subset).
const SEED_RELAY_SUBSET: usize = 3;
/// Events timestamped more than this far into the future are rejected; clock
/// skew between author and proxy is a few seconds, five minutes is generous.
const FUTURE_EVENT_SKEW_SECS: u64 = 5 * 60;
/// Maximum length of attacker-controlled values before they reach a log line.
const MAX_LOG_VALUE_CHARS: usize = 128;

/// Seed relays for fetching user server lists (kind 10063)
const SEED_RELAYS: &[&str] = &[
    "wss://nos.lol",
    "wss://nostr.mom",
    "wss://purplepag.es",
    "wss://relay.nostr.band",
    "wss://relay.snort.social",
    "wss://relay.primal.net",
    "wss://no.str.cr",
    "wss://nostr21.com",
    "wss://nostrue.com",
    "wss://purplerelay.com",
];

/// Classification of a failed candidate URL for negative caching.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CandidateFailureClass {
    Missing,
    Forbidden,
    Permanent,
    Transient,
}

impl CandidateFailureClass {
    pub(crate) fn from_status(status: u16) -> Self {
        match status {
            404 | 410 => Self::Missing,
            401 | 403 => Self::Forbidden,
            408 | 429 => Self::Transient,
            300..=499 => Self::Permanent,
            _ => Self::Transient,
        }
    }

    pub(crate) fn from_error(error: &crate::error::SvcError) -> Self {
        match error {
            crate::error::SvcError::UpstreamError(status) => Self::from_status(*status),
            _ => Self::Transient,
        }
    }
}

/// Aggregates all candidate failure classes into one stable client response.
#[derive(Default)]
pub struct CandidateFailureSummary {
    classes: HashSet<CandidateFailureClass>,
}

impl CandidateFailureSummary {
    pub fn record(&mut self, class: CandidateFailureClass) {
        self.classes.insert(class);
    }

    pub fn into_error(self) -> crate::error::SvcError {
        use crate::error::SvcError;

        match self.classes.len() {
            0 => SvcError::UpstreamError(404),
            1 if self.classes.contains(&CandidateFailureClass::Missing) => {
                SvcError::UpstreamError(404)
            }
            1 if self.classes.contains(&CandidateFailureClass::Forbidden) => {
                SvcError::UpstreamError(403)
            }
            _ => SvcError::UpstreamError(502),
        }
    }
}

/// An in-memory cache entry for a failed blob candidate URL.
#[derive(Clone, Copy, Debug)]
struct CandidateFailureCacheEntry {
    class: CandidateFailureClass,
    expires_at: Instant,
}

/// Bounded, in-memory negative cache for immutable blob candidate URLs.
///
/// Keys combine the expected blob hash and the candidate URL: a hash-specific
/// mismatch (502) must not poison the URL for other hashes, and the video path
/// shares this cache with the image path.
#[derive(Clone)]
pub struct CandidateFailureCache {
    entries: Arc<RwLock<HashMap<(String, String), CandidateFailureCacheEntry>>>,
    not_found_ttl: Duration,
    permanent_ttl: Duration,
    transient_ttl: Duration,
}

const MAX_CANDIDATE_FAILURE_CACHE_ENTRIES: usize = 10_000;

impl CandidateFailureCache {
    pub fn new(not_found_ttl: Duration, permanent_ttl: Duration, transient_ttl: Duration) -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            not_found_ttl,
            permanent_ttl,
            transient_ttl,
        }
    }

    fn ttl_for(&self, class: CandidateFailureClass) -> Duration {
        match class {
            CandidateFailureClass::Missing => self.not_found_ttl,
            CandidateFailureClass::Forbidden | CandidateFailureClass::Permanent => {
                self.permanent_ttl
            }
            CandidateFailureClass::Transient => self.transient_ttl,
        }
    }

    /// Return an unexpired cached class for a (hash, URL) pair.
    ///
    /// The read path only takes a shared lock and checks expiry for the one
    /// requested entry; the full expiry sweep is confined to the write path so
    /// a cache hit never pays for sweeping up to the whole map.
    pub async fn lookup(&self, hash: &str, url: &str) -> Option<CandidateFailureClass> {
        let now = Instant::now();
        let key = (hash.to_ascii_lowercase(), url.to_string());
        let entries = self.entries.read().await;
        entries
            .get(&key)
            .filter(|entry| entry.expires_at > now)
            .map(|entry| entry.class)
    }

    /// Remember a candidate failure unless its class is explicitly disabled with TTL zero.
    ///
    /// The key combines the expected blob hash and the candidate URL, so a
    /// hash-specific mismatch cannot lock a legitimate URL out for its real
    /// hash (the image and video paths share this cache).
    pub async fn remember(&self, hash: &str, url: &str, class: CandidateFailureClass) {
        let ttl = self.ttl_for(class);
        if ttl.is_zero() {
            return;
        }

        let now = Instant::now();
        let Some(expires_at) = now.checked_add(ttl) else {
            return;
        };
        let key = (hash.to_ascii_lowercase(), url.to_string());
        let mut entries = self.entries.write().await;
        // Opportunistic sweep: expired entries are freed when we are already
        // holding the write lock, never on the hot read path.
        entries.retain(|_, entry| entry.expires_at > now);

        if !entries.contains_key(&key) && entries.len() >= MAX_CANDIDATE_FAILURE_CACHE_ENTRIES {
            if let Some(oldest_key) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(key, _)| key.clone())
            {
                entries.remove(&oldest_key);
            }
        }

        entries.insert(key, CandidateFailureCacheEntry { class, expires_at });
    }
}

/// Cache entry for a resolved author server list.
#[derive(Clone, Debug)]
struct AuthorServerCacheEntry {
    result: AuthorServerLookup,
    cached_at: Instant,
}

/// Cached outcome of an author server-list lookup.
#[derive(Clone, Debug)]
enum AuthorServerLookup {
    Servers(Vec<String>),
    Failed,
}

/// Cached outcome of a NIP-94 location lookup by blob hash.
#[derive(Clone, Debug)]
enum BlobLocationLookup {
    Urls(Vec<String>),
    Failed,
}

/// Cache entry for NIP-94 locations discovered by blob hash.
#[derive(Clone, Debug)]
struct BlobLocationCacheEntry {
    result: BlobLocationLookup,
    cached_at: Instant,
}
/// State for Blossom server resolution with caching.
pub struct BlossomState {
    /// Cache of author pubkey -> server list outcome.
    server_list_cache: Arc<RwLock<HashMap<PublicKey, AuthorServerCacheEntry>>>,
    /// Cache of blob hash -> NIP-94 locations.
    blob_location_cache: Arc<RwLock<HashMap<String, BlobLocationCacheEntry>>>,
    /// Failed candidate URLs, bounded in-memory and scoped to their failure class TTL.
    candidate_failure_cache: CandidateFailureCache,
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
    /// Create Blossom resolution state with independent cache TTLs.
    pub async fn new(
        cache_ttl: Duration,
        failure_cache_ttl: Duration,
        discovery_cache_ttl: Duration,
        discovery_timeout: Duration,
        candidate_failure_cache: CandidateFailureCache,
    ) -> Self {
        // Initialize Nostr client with seed relays.
        //
        // `Client::default()` leaves `verify_subscriptions = false` (nostr-sdk
        // default), meaning relays may deliver arbitrary *signed* events that
        // do not match our subscription filter. Verification against the
        // requested filter — plus banning relays that violate it — turns a
        // hostile seed relay from an attacker-controlled event source into a
        // relay that is simply excluded from future lookups.
        let client = Client::builder()
            .verify_subscriptions(true)
            .ban_relay_on_mismatch(true)
            .build();

        for relay in SEED_RELAYS {
            if let Err(e) = client.add_relay(*relay).await {
                warn!("Failed to add relay {}: {:?}", relay, e);
            }
        }

        // `add_relay` only registers a relay; a relay stays in the "initialized"
        // state until `connect()` is called, and queries against it fail with
        // "relay is initialized but not ready". `connect()` is non-blocking: it
        // spawns background tasks that dial and auto-reconnect.
        client.connect().await;

        Self {
            server_list_cache: Arc::new(RwLock::new(HashMap::new())),
            blob_location_cache: Arc::new(RwLock::new(HashMap::new())),
            cache_ttl,
            failure_cache_ttl,
            candidate_failure_cache,
            discovery_cache_ttl,
            discovery_timeout,
            client,
        }
    }

    /// Access the shared negative cache for blob candidate URLs.
    pub fn candidate_failure_cache(&self) -> &CandidateFailureCache {
        &self.candidate_failure_cache
    }

    async fn fetch_events(
        &self,
        filter: Filter,
        query_key: &str,
    ) -> Result<BTreeSet<Event>, String> {
        // The fetch builder bounds the relay request by `discovery_timeout`.
        // The outer timeout remains a backstop for the SDK hanging, so it must
        // be strictly longer — arming both at the same instant makes them race
        // and discards results that the inner call was about to return.
        let backstop = self.discovery_timeout + Duration::from_secs(2);

        // Query only a small deterministic subset of the seed relays instead of
        // all ten. `as=` and blob discovery together trigger two lookups per
        // HTTP request; fanning both out to every seed would open twenty relay
        // connections per request — cheap for an attacker to amplify. A
        // per-key rotation keeps the subset stable for repeated lookups of the
        // same key (cache-friendly) while spreading load across all seeds, and
        // the seed list is short enough that a ban still leaves plenty of
        // redundancy.
        let mut hasher = DefaultHasher::new();
        query_key.hash(&mut hasher);
        let start = (hasher.finish() as usize) % SEED_RELAYS.len();
        let targets = (0..SEED_RELAY_SUBSET)
            .map(|offset| {
                let relay = SEED_RELAYS[(start + offset) % SEED_RELAYS.len()];
                (relay, vec![filter.clone()])
            })
            .collect::<Vec<_>>();

        tokio::time::timeout(
            backstop,
            self.client
                .fetch_events(targets)
                .timeout(self.discovery_timeout),
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
                    .kind(Kind::from(KIND_SERVER_LIST))
                    .author(*pubkey)
                    .limit(10),
                &pubkey.to_hex(),
            )
            .await?;

        // Defensive post-filter: a hostile seed relay can deliver signed events
        // outside the requested filter, so only events that are genuinely the
        // author's kind-10063 list may influence the server list.
        let servers = match best_event(authoritative_server_list_events(events.iter(), pubkey)) {
            Some(event) => servers_from_event(event),
            None => {
                debug!("No server list events found for pubkey {}", pubkey);
                Vec::new()
            }
        };

        // Count only; the servers themselves are attacker-influenced and the
        // author can publish arbitrarily many of them.
        debug!("Found {} servers for pubkey {}", servers.len(), pubkey);
        Ok(servers)
    }

    /// Get an author's kind-10063 server list with separate success and failure TTLs.
    pub async fn get_author_servers(&self, pubkey_str: &str) -> Result<Vec<String>, String> {
        let pubkey = parse_pubkey(pubkey_str)?;

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
                let ttl = match &entry.result {
                    BlobLocationLookup::Urls(_) => self.discovery_cache_ttl,
                    BlobLocationLookup::Failed => self.failure_cache_ttl,
                };
                if entry.cached_at.elapsed() < ttl {
                    return match &entry.result {
                        BlobLocationLookup::Urls(urls) => Ok(urls.clone()),
                        BlobLocationLookup::Failed => {
                            Err("cached blob discovery failure".to_string())
                        }
                    };
                }
            }
        }

        let x_tag = SingleLetterTag::from_char('x')
            .expect("lowercase x is a valid Nostr single-letter tag");
        let events = match self
            .fetch_events(
                Filter::new()
                    .kind(Kind::from(KIND_FILE_METADATA))
                    .custom_tag(x_tag, normalized_hash.clone())
                    .limit(20),
                &normalized_hash,
            )
            .await
        {
            Ok(events) => events,
            Err(error) => {
                // Negative-cache the failure (mirroring `get_author_servers`)
                // so a dead relay fan-out is not repeated on every request.
                warn!("Blob discovery failed for {}: {}", normalized_hash, error);
                self.blob_location_cache.write().await.insert(
                    normalized_hash,
                    BlobLocationCacheEntry {
                        result: BlobLocationLookup::Failed,
                        cached_at: Instant::now(),
                    },
                );
                return Err(error);
            }
        };

        // Only events that actually attest to the requested hash may contribute
        // URLs; relays can publish kind-1063 events for arbitrary hashes.
        let urls = blob_urls_from_events(events.iter(), &normalized_hash);

        self.blob_location_cache.write().await.insert(
            normalized_hash,
            BlobLocationCacheEntry {
                result: BlobLocationLookup::Urls(urls.clone()),
                cached_at: Instant::now(),
            },
        );
        Ok(urls)
    }
}

// ---------------------------------------------------------------------------
// Nostr event parsing (pure, testable; shared with integration tests)
// ---------------------------------------------------------------------------

/// Parse a Nostr public key from either bech32 (`npub…`) or lowercase hex.
pub fn parse_pubkey(pubkey_str: &str) -> Result<PublicKey, String> {
    if let Ok(pubkey) = PublicKey::from_bech32(pubkey_str) {
        return Ok(pubkey);
    }
    if let Ok(pubkey) = PublicKey::from_hex(pubkey_str) {
        return Ok(pubkey);
    }
    // `pubkey_str` is attacker-controlled (query parameter); never log it raw.
    Err(format!(
        "Invalid pubkey format: {}",
        truncate_for_log(pubkey_str)
    ))
}

/// Select the most recent event by `created_at`, breaking ties deterministically.
pub fn best_event<'a, I>(events: I) -> Option<&'a Event>
where
    I: IntoIterator<Item = &'a Event>,
{
    events.into_iter().max_by_key(|event| event.created_at)
}

/// Filter events down to authoritative kind-10063 server lists for a pubkey.
///
/// A hostile seed relay can deliver any signed event it likes, so only events
/// with exactly `KIND_SERVER_LIST`, signed by exactly `pubkey`, and not
/// timestamped more than `FUTURE_EVENT_SKEW_SECS` into the future may feed
/// `best_event` / `servers_from_event`. The future check tolerates small clock
/// skew while rejecting events whose `created_at` has not happened yet.
pub fn authoritative_server_list_events<'a, I>(events: I, pubkey: &PublicKey) -> Vec<&'a Event>
where
    I: IntoIterator<Item = &'a Event>,
{
    let horizon = Timestamp::now()
        .as_secs()
        .saturating_add(FUTURE_EVENT_SKEW_SECS);
    events
        .into_iter()
        .filter(|event| event.kind == Kind::from(KIND_SERVER_LIST) && event.pubkey == *pubkey)
        .filter(|event| event.created_at.as_secs() <= horizon)
        .collect()
}

/// Cap attacker-controlled values at a sane length before they reach logs.
fn truncate_for_log(value: &str) -> String {
    match value.char_indices().nth(MAX_LOG_VALUE_CHARS) {
        Some((idx, _)) => format!("{}…", &value[..idx]),
        None => value.to_string(),
    }
}

/// Extract and normalize BUD-03 `server` tags from a single kind-10063 event.
/// Tags with fewer than two entries or a non-`server` name are skipped.
pub fn servers_from_event(event: &Event) -> Vec<String> {
    event
        .tags
        .iter()
        .filter_map(|tag| {
            let parts = tag.clone().to_vec();
            (parts.len() >= 2 && parts[0] == "server").then(|| normalize_server_url(&parts[1]))
        })
        .collect()
}

/// Extract deduplicated, SSRF-safe blob URLs from kind-1063 `url` and `fallback`
/// tags. Order follows the source events; private/loopback hosts are filtered.
///
/// Only events carrying an `x` tag matching `expected_hash` contribute: relays
/// can publish kind-1063 events for arbitrary hashes, and a hash-addressed blob
/// must never be resolved through an event that attests to a different hash.
pub fn blob_urls_from_events<'a, I>(events: I, expected_hash: &str) -> Vec<String>
where
    I: IntoIterator<Item = &'a Event>,
{
    let expected_hash = expected_hash.to_ascii_lowercase();
    let mut urls = Vec::new();
    let mut seen = HashSet::new();
    for event in events {
        let attests_to_hash = event.tags.iter().any(|tag| {
            let parts = tag.clone().to_vec();
            parts.len() >= 2 && parts[0] == "x" && parts[1].to_ascii_lowercase() == expected_hash
        });
        if !attests_to_hash {
            continue;
        }
        for tag in event.tags.iter() {
            let parts = tag.clone().to_vec();
            if parts.len() < 2 || !matches!(parts[0].as_str(), "url" | "fallback") {
                continue;
            }
            let url = &parts[1];
            if !is_allowed_untrusted_server(url) {
                continue;
            }
            if seen.insert(url.clone()) {
                urls.push(url.clone());
            }
        }
    }
    urls
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
///
/// `max_server_hints` caps how many `xs=` hints are honoured: the query string
/// can carry arbitrarily many hints, and each one becomes a candidate host the
/// proxy may contact. Surplus hints are dropped (truncate), not rejected, so
/// the API stays compatible.
pub fn combine_server_lists(
    xs_servers: Option<&[String]>,
    as_servers: Option<&[String]>,
    fallback_servers: &[String],
    max_server_hints: usize,
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();

    let mut add_servers = |servers: &[String]| {
        for server in servers {
            let normalized = normalize_server_url(server);
            if !is_allowed_untrusted_server(&normalized) {
                // `server` is attacker-controlled; never log it raw.
                warn!(
                    "Ignoring private or invalid Blossom upstream: {}",
                    truncate_for_log(server)
                );
                continue;
            }
            let lowercase = normalized.to_lowercase();
            if seen.insert(lowercase) {
                result.push(normalized);
            }
        }
    };

    if let Some(xs) = xs_servers {
        let honored = xs.len().min(max_server_hints);
        add_servers(&xs[..honored]);
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
    max_bytes: usize,
    fetch_timeout: Duration,
) -> Result<bytes::Bytes, crate::error::SvcError> {
    use crate::error::SvcError;

    if !is_allowed_untrusted_server(url) {
        return Err(SvcError::UpstreamError(400));
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(SvcError::UpstreamError(504));
    }
    // Cap one candidate's share of the shared deadline (same pattern as the
    // video path's `remaining.min(ffmpeg_timeout)`): otherwise a single
    // stalled host would consume the whole 15 s budget and starve failover.
    let attempt_timeout = remaining.min(fetch_timeout);

    tokio::time::timeout(attempt_timeout, async {
        let response = http.get(url).send().await?;
        if !response.status().is_success() {
            return Err(SvcError::UpstreamError(response.status().as_u16()));
        }
        crate::fetch::read_body_capped(response, max_bytes).await
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
/// All candidates share one deadline, every body is capped at `max_bytes`, and
/// successful bytes must match `hash`. `max_blob_candidates` bounds the total
/// fan-out a single request can aim at third-party hosts; `fetch_timeout` caps
/// each individual attempt so one stalled host cannot consume the shared
/// deadline.
#[allow(clippy::too_many_arguments)]
pub async fn fetch_blob(
    http: &reqwest::Client,
    candidate_failure_cache: &CandidateFailureCache,
    servers: &[String],
    discovered_urls: &[String],
    hash: &str,
    ext: Option<&str>,
    deadline: Instant,
    max_bytes: usize,
    max_blob_candidates: usize,
    fetch_timeout: Duration,
) -> Result<bytes::Bytes, crate::error::SvcError> {
    fetch_blob_inner(
        http,
        candidate_failure_cache,
        servers,
        discovered_urls,
        hash,
        ext,
        deadline,
        max_bytes,
        max_blob_candidates,
        fetch_timeout,
        true,
    )
    .await
}

/// Attempt a bounded, fully hash-verified download of a blob that may simply
/// be too large to verify economically (the video-thumbnail path's use case:
/// a candidate over `max_bytes` fails fast on `Content-Length` before any
/// body is read). Consults the negative candidate cache to skip known-dead
/// candidates, but — unlike [`fetch_blob`] — never *writes* to it: "too large
/// to verify" or "didn't finish within budget" says nothing about candidate
/// health, and recording it here would poison the very next call for the
/// same request — the range-probed fallback, designed for exactly this case
/// — into skipping the same candidate and failing outright.
#[allow(clippy::too_many_arguments)]
pub async fn try_fetch_verified_blob(
    http: &reqwest::Client,
    candidate_failure_cache: &CandidateFailureCache,
    servers: &[String],
    discovered_urls: &[String],
    hash: &str,
    ext: Option<&str>,
    deadline: Instant,
    max_bytes: usize,
    max_blob_candidates: usize,
    fetch_timeout: Duration,
) -> Option<bytes::Bytes> {
    fetch_blob_inner(
        http,
        candidate_failure_cache,
        servers,
        discovered_urls,
        hash,
        ext,
        deadline,
        max_bytes,
        max_blob_candidates,
        fetch_timeout,
        false,
    )
    .await
    .ok()
}

#[allow(clippy::too_many_arguments)]
async fn fetch_blob_inner(
    http: &reqwest::Client,
    candidate_failure_cache: &CandidateFailureCache,
    servers: &[String],
    discovered_urls: &[String],
    hash: &str,
    ext: Option<&str>,
    deadline: Instant,
    max_bytes: usize,
    max_blob_candidates: usize,
    fetch_timeout: Duration,
    record_failures: bool,
) -> Result<bytes::Bytes, crate::error::SvcError> {
    let mut candidates = Vec::with_capacity(servers.len() + discovered_urls.len());
    let mut seen = HashSet::new();

    for server in servers {
        let url = blob_url(server, hash, ext);
        if seen.insert(url.clone()) {
            candidates.push(url);
        }
    }
    for url in discovered_urls {
        if seen.insert(url.clone()) {
            candidates.push(url.clone());
        }
    }
    // Truncate (not reject) the combined list so a single request can never
    // turn the proxy into a reflector for an unbounded set of hosts; the
    // highest-priority candidates (hints, author servers, fallbacks) survive.
    candidates.truncate(max_blob_candidates);

    let mut failures = CandidateFailureSummary::default();
    let mut last_error = crate::error::SvcError::UpstreamError(404);
    let mut attempted = 0;
    let mut skipped = 0;

    for url in candidates {
        if let Some(class) = candidate_failure_cache.lookup(hash, &url).await {
            crate::metrics::record_cache_hit("blossom_negative");
            debug!(
                ?class,
                candidate = %truncate_for_log(&url),
                "skipping negatively cached blob candidate"
            );
            failures.record(class);
            skipped += 1;
            continue;
        }

        attempted += 1;
        match fetch_candidate(http, &url, hash, deadline, max_bytes, fetch_timeout).await {
            Ok(bytes) => return Ok(bytes),
            Err(error) => {
                let class = CandidateFailureClass::from_error(&error);
                debug!(
                    ?class,
                    candidate = %truncate_for_log(&url),
                    ?error,
                    record_failures,
                    "blob candidate failed"
                );
                if record_failures {
                    candidate_failure_cache.remember(hash, &url, class).await;
                }
                failures.record(class);
                last_error = error;
            }
        }
    }

    if record_failures {
        warn!(
            attempted,
            skipped, "all blob candidates failed for {}", hash
        );
    }
    Err(if attempted == 0 {
        failures.into_error()
    } else {
        last_error
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SvcError;
    use axum::{http::StatusCode, routing::get, Router};
    use std::{
        net::SocketAddr,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };

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
    async fn spawn_status_server(status: StatusCode, requests: Arc<AtomicUsize>) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app = Router::new().fallback(get(move || {
            let requests = requests.clone();
            async move {
                requests.fetch_add(1, Ordering::Relaxed);
                status
            }
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

        let combined = combine_server_lists(Some(&xs), Some(&as_s), &fallback, 4);

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

        let combined = combine_server_lists(Some(&xs), Some(&author_servers), &[], 4);

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
    #[test]
    fn candidate_failure_summary_returns_deterministic_error() {
        let mut missing = CandidateFailureSummary::default();
        missing.record(CandidateFailureClass::Missing);
        assert!(matches!(missing.into_error(), SvcError::UpstreamError(404)));

        let mut forbidden = CandidateFailureSummary::default();
        forbidden.record(CandidateFailureClass::Forbidden);
        assert!(matches!(
            forbidden.into_error(),
            SvcError::UpstreamError(403)
        ));

        let mut mixed = CandidateFailureSummary::default();
        mixed.record(CandidateFailureClass::Missing);
        mixed.record(CandidateFailureClass::Transient);
        assert!(matches!(mixed.into_error(), SvcError::UpstreamError(502)));
        assert_eq!(
            CandidateFailureClass::from_status(429),
            CandidateFailureClass::Transient
        );
    }

    #[tokio::test]
    async fn candidate_failure_cache_zero_ttl_does_not_store_failure() {
        let cache = CandidateFailureCache::new(Duration::ZERO, Duration::ZERO, Duration::ZERO);
        cache
            .remember(
                &"a".repeat(64),
                "https://missing.example/blob.jpg",
                CandidateFailureClass::Missing,
            )
            .await;

        assert_eq!(
            cache
                .lookup(&"a".repeat(64), "https://missing.example/blob.jpg")
                .await,
            None
        );
    }

    #[tokio::test]
    async fn candidate_failure_cache_evicts_earliest_expiring_entry_at_capacity() {
        let cache = CandidateFailureCache::new(
            Duration::from_secs(1),
            Duration::from_secs(60),
            Duration::from_secs(60),
        );
        for index in 0..MAX_CANDIDATE_FAILURE_CACHE_ENTRIES {
            cache
                .remember(
                    &"a".repeat(64),
                    &format!("https://missing.example/{index}"),
                    CandidateFailureClass::Transient,
                )
                .await;
        }
        cache
            .remember(
                &"a".repeat(64),
                "https://missing.example/new",
                CandidateFailureClass::Missing,
            )
            .await;

        let entries = cache.entries.read().await;
        assert_eq!(entries.len(), MAX_CANDIDATE_FAILURE_CACHE_ENTRIES);
        assert!(entries.contains_key(&("a".repeat(64), "https://missing.example/new".to_string())));
    }

    #[tokio::test]
    async fn fetch_blob_skips_negatively_cached_missing_candidate() {
        let expected_bytes = b"verified blob".to_vec();
        let hash = hex::encode(Sha256::digest(&expected_bytes));
        let requests = Arc::new(AtomicUsize::new(0));
        let missing_server = spawn_status_server(StatusCode::NOT_FOUND, requests.clone()).await;
        let verified_server = spawn_blob_server(expected_bytes.clone()).await;
        crate::init_crypto_provider();
        let http = reqwest::Client::builder()
            .resolve("missing.example", missing_server)
            .resolve("verified.example", verified_server)
            .build()
            .unwrap();
        let missing = format!("http://missing.example:{}", missing_server.port());
        let verified = format!("http://verified.example:{}", verified_server.port());
        let cache = CandidateFailureCache::new(
            Duration::from_secs(60),
            Duration::from_secs(60),
            Duration::from_secs(60),
        );

        let first = fetch_blob(
            &http,
            &cache,
            std::slice::from_ref(&missing),
            &[],
            &hash,
            Some("bin"),
            Instant::now() + Duration::from_secs(1),
            1024 * 1024,
            8,
            Duration::from_secs(5),
        )
        .await;
        assert!(matches!(first, Err(SvcError::UpstreamError(404))));

        let bytes = fetch_blob(
            &http,
            &cache,
            &[missing, verified],
            &[],
            &hash,
            Some("bin"),
            Instant::now() + Duration::from_secs(1),
            1024 * 1024,
            8,
            Duration::from_secs(5),
        )
        .await
        .unwrap();

        assert_eq!(bytes.as_ref(), expected_bytes);
        assert_eq!(requests.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn fetch_blob_skips_corrupt_candidate() {
        let expected_bytes = b"verified blob".to_vec();
        let hash = hex::encode(Sha256::digest(&expected_bytes));
        let corrupt_server = spawn_blob_server(b"corrupt blob".to_vec()).await;
        let verified_server = spawn_blob_server(expected_bytes.clone()).await;
        crate::init_crypto_provider();
        let http = reqwest::Client::builder()
            .resolve("corrupt.example", corrupt_server)
            .resolve("verified.example", verified_server)
            .build()
            .unwrap();
        let servers = vec![
            format!("http://corrupt.example:{}", corrupt_server.port()),
            format!("http://verified.example:{}", verified_server.port()),
        ];
        let failure_cache = CandidateFailureCache::new(
            Duration::from_secs(60),
            Duration::from_secs(60),
            Duration::from_secs(60),
        );

        let bytes = fetch_blob(
            &http,
            &failure_cache,
            &servers,
            &[],
            &hash,
            Some("bin"),
            Instant::now() + Duration::from_secs(1),
            1024 * 1024,
            8,
            Duration::from_secs(5),
        )
        .await
        .unwrap();

        assert_eq!(bytes.as_ref(), expected_bytes);
    }

    #[tokio::test]
    async fn fetch_blob_honors_aggregate_deadline() {
        crate::init_crypto_provider();
        let failure_cache = CandidateFailureCache::new(
            Duration::from_secs(60),
            Duration::from_secs(60),
            Duration::from_secs(60),
        );
        let result = fetch_blob(
            &reqwest::Client::new(),
            &failure_cache,
            &["https://cdn.example.com".to_string()],
            &[],
            &"a".repeat(64),
            Some("jpg"),
            Instant::now(),
            1024 * 1024,
            8,
            Duration::from_secs(5),
        )
        .await;

        assert!(matches!(result, Err(SvcError::UpstreamError(504))));
    }

    #[tokio::test]
    async fn try_fetch_verified_blob_returns_hash_verified_bytes() {
        let expected_bytes = b"verified video blob".to_vec();
        let hash = hex::encode(Sha256::digest(&expected_bytes));
        let server = spawn_blob_server(expected_bytes.clone()).await;
        crate::init_crypto_provider();
        let http = reqwest::Client::builder()
            .resolve("video.example", server)
            .build()
            .unwrap();
        let servers = vec![format!("http://video.example:{}", server.port())];
        let failure_cache = CandidateFailureCache::new(
            Duration::from_secs(60),
            Duration::from_secs(60),
            Duration::from_secs(60),
        );

        let bytes = try_fetch_verified_blob(
            &http,
            &failure_cache,
            &servers,
            &[],
            &hash,
            Some("mp4"),
            Instant::now() + Duration::from_secs(1),
            1024 * 1024,
            8,
            Duration::from_secs(5),
        )
        .await;

        assert_eq!(bytes.map(|b| b.to_vec()), Some(expected_bytes));
    }

    #[tokio::test]
    async fn try_fetch_verified_blob_does_not_poison_the_negative_cache_on_failure() {
        // A candidate whose body exceeds `max_bytes` fails fast on
        // `Content-Length`. Unlike `fetch_blob`, that failure must never be
        // written to the shared negative cache — the range-probe fallback
        // that runs next for the same request is designed for exactly this
        // "too large" case, and would otherwise find the same candidate
        // pre-poisoned and skip it, breaking every request for that video.
        let expected_bytes = vec![b'x'; 4096];
        let hash = hex::encode(Sha256::digest(&expected_bytes));
        let server = spawn_blob_server(expected_bytes.clone()).await;
        crate::init_crypto_provider();
        let http = reqwest::Client::builder()
            .resolve("big.example", server)
            .build()
            .unwrap();
        let candidate = format!("http://big.example:{}", server.port());
        let servers = vec![candidate.clone()];
        let failure_cache = CandidateFailureCache::new(
            Duration::from_secs(60),
            Duration::from_secs(60),
            Duration::from_secs(60),
        );

        let bytes = try_fetch_verified_blob(
            &http,
            &failure_cache,
            &servers,
            &[],
            &hash,
            Some("mp4"),
            Instant::now() + Duration::from_secs(1),
            1024, // smaller than the 4096-byte body: fails fast on Content-Length
            8,
            Duration::from_secs(5),
        )
        .await;

        assert!(bytes.is_none(), "oversized candidate must not verify");
        assert_eq!(
            failure_cache.lookup(&hash, &candidate).await,
            None,
            "a size-bounded verify failure must not poison the negative cache"
        );
    }
}
