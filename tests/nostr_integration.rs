//! Integration tests for the Nostr-facing side of Blossom resolution.
//!
//! These build *real*, signed Nostr events with `nostr-sdk` and push them
//! through the same parsing helpers the relay code path uses, so the tests
//! break if an SDK upgrade changes event/tag semantics.
//!
//! Tests that touch the public relay network are marked `#[ignore]`; run them
//! with `cargo test -- --ignored --test-threads=1`.

use nostr_sdk::prelude::*;
use rust_imgproxy::blossom::{
    authoritative_server_list_events, best_event, blob_urls_from_events, combine_server_lists,
    extract_blossom_hash, normalize_server_url, parse_blossom_filename, parse_pubkey,
    servers_from_event,
};

const KIND_SERVER_LIST: u16 = 10063;
const KIND_FILE_METADATA: u16 = 1063;
async fn fetch_events_from(
    client: &Client,
    relays: &[&str],
    filter: Filter,
    timeout: std::time::Duration,
) -> Result<std::collections::BTreeSet<Event>, nostr_sdk::error::Error> {
    let targets = relays
        .iter()
        .map(|relay| (*relay, vec![filter.clone()]))
        .collect::<Vec<_>>();
    client.fetch_events(targets).timeout(timeout).await
}

/// Build a signed kind-10063 (BUD-03) server-list event.
fn server_list_event(keys: &Keys, servers: &[&str]) -> Event {
    let tags = servers
        .iter()
        .map(|server| Tag::parse(["server", server]).expect("server tag"));
    EventBuilder::new(Kind::from(KIND_SERVER_LIST), "")
        .tags(tags)
        .finalize(keys)
        .expect("sign server list event")
}

/// Build a signed kind-1063 (NIP-94) file-metadata event.
fn file_metadata_event(keys: &Keys, hash: &str, tags: &[(&str, &str)]) -> Event {
    let mut built = vec![Tag::parse(["x", hash]).expect("x tag")];
    built.extend(
        tags.iter()
            .map(|(name, value)| Tag::parse([*name, *value]).expect("tag")),
    );
    EventBuilder::new(Kind::from(KIND_FILE_METADATA), "")
        .tags(built)
        .finalize(keys)
        .expect("sign file metadata event")
}

fn at(keys: &Keys, servers: &[&str], created_at: u64) -> Event {
    let tags = servers
        .iter()
        .map(|server| Tag::parse(["server", server]).expect("server tag"));
    EventBuilder::new(Kind::from(KIND_SERVER_LIST), "")
        .tags(tags)
        .custom_created_at(Timestamp::from(created_at))
        .finalize(keys)
        .expect("sign server list event")
}

// ---------------------------------------------------------------------------
// Pubkey parsing
// ---------------------------------------------------------------------------

#[test]
fn parse_pubkey_accepts_hex_and_bech32_for_same_key() {
    let keys = Keys::generate();
    let pubkey = keys.public_key();

    let from_hex = parse_pubkey(&pubkey.to_hex()).expect("hex parses");
    let from_bech32 = parse_pubkey(&pubkey.to_bech32().expect("bech32")).expect("bech32 parses");

    assert_eq!(from_hex, pubkey);
    assert_eq!(from_bech32, pubkey);
    assert_eq!(from_hex, from_bech32);
}

#[test]
fn parse_pubkey_rejects_malformed_input() {
    for bad in [
        "",
        "not-a-key",
        "npub1invalid",
        // 63 hex chars: one short of a valid key.
        &"a".repeat(63),
        // Valid length but non-hex.
        &"z".repeat(64),
    ] {
        assert!(
            parse_pubkey(bad).is_err(),
            "expected {bad:?} to be rejected"
        );
    }
}

// ---------------------------------------------------------------------------
// kind 10063 — BUD-03 server lists
// ---------------------------------------------------------------------------

#[test]
fn servers_from_event_extracts_and_normalizes_server_tags() {
    let keys = Keys::generate();
    let event = server_list_event(
        &keys,
        &[
            "https://cdn.example.com",
            // No scheme: normalization must add https://.
            "blossom.example.org",
            // Trailing slash must be stripped.
            "https://media.example.net/",
        ],
    );

    assert_eq!(
        servers_from_event(&event),
        vec![
            "https://cdn.example.com".to_string(),
            "https://blossom.example.org".to_string(),
            "https://media.example.net".to_string(),
        ]
    );
}

#[test]
fn servers_from_event_preserves_relay_order() {
    let keys = Keys::generate();
    let event = server_list_event(&keys, &["https://first.example", "https://second.example"]);

    // Order is meaningful: BUD-03 lists servers by author preference.
    assert_eq!(
        servers_from_event(&event),
        vec![
            "https://first.example".to_string(),
            "https://second.example".to_string()
        ]
    );
}

#[test]
fn servers_from_event_ignores_unrelated_and_valueless_tags() {
    let keys = Keys::generate();
    let event = EventBuilder::new(Kind::from(KIND_SERVER_LIST), "")
        .tags([
            Tag::parse(["server", "https://kept.example"]).unwrap(),
            // Wrong tag name.
            Tag::parse(["relay", "wss://relay.example"]).unwrap(),
            // Right name, but no value.
            Tag::parse(["server"]).unwrap(),
            Tag::parse(["client", "nostube"]).unwrap(),
        ])
        .finalize(&keys)
        .expect("sign");

    assert_eq!(
        servers_from_event(&event),
        vec!["https://kept.example".to_string()]
    );
}

#[test]
fn servers_from_event_returns_empty_for_event_without_server_tags() {
    let keys = Keys::generate();
    let event = server_list_event(&keys, &[]);
    assert!(servers_from_event(&event).is_empty());
}

// ---------------------------------------------------------------------------
// best_event — newest-wins replaceable-event selection
// ---------------------------------------------------------------------------

#[test]
fn best_event_selects_newest_by_created_at() {
    let keys = Keys::generate();
    let older = at(&keys, &["https://old.example"], 1_000);
    let newer = at(&keys, &["https://new.example"], 2_000);
    let middle = at(&keys, &["https://mid.example"], 1_500);

    // Deliberately unordered: selection must not depend on input order.
    let events = [older, newer.clone(), middle];
    let chosen = best_event(events.iter()).expect("an event is selected");

    assert_eq!(chosen.id, newer.id);
    assert_eq!(
        servers_from_event(chosen),
        vec!["https://new.example".to_string()]
    );
}

#[test]
fn best_event_returns_none_for_empty_input() {
    let events: Vec<Event> = Vec::new();
    assert!(best_event(events.iter()).is_none());
}

#[test]
fn best_event_on_single_event_returns_that_event() {
    let keys = Keys::generate();
    let only = at(&keys, &["https://only.example"], 42);
    let events = [only.clone()];
    assert_eq!(best_event(events.iter()).unwrap().id, only.id);
}

// ---------------------------------------------------------------------------
// authoritative_server_list_events — defensive kind/author/timestamp filter
// ---------------------------------------------------------------------------

#[test]
fn authoritative_server_list_events_ignores_foreign_author_and_kind() {
    let author = Keys::generate();
    let attacker = Keys::generate();
    let now = Timestamp::now().as_secs();

    let own = server_list_event(&author, &["https://own.example"]);
    // A hostile relay can sign and deliver any event: same kind but a foreign
    // author must not influence the list…
    let foreign_author = server_list_event(&attacker, &["https://attacker.example"]);
    // …and the same author publishing a *different* kind must not either.
    let foreign_kind = file_metadata_event(
        &author,
        &"a".repeat(64),
        &[("url", "https://not-a-server-list.example/blob.mp4")],
    );
    // Newest wins among the *authoritative* events only.
    let newer_own = at(&author, &["https://own-newer.example"], now + 10);

    let events = [own, foreign_author, foreign_kind, newer_own.clone()];
    let authoritative = authoritative_server_list_events(events.iter(), &author.public_key());

    assert_eq!(
        authoritative.len(),
        2,
        "only the author's own kind-10063 events"
    );
    let chosen = best_event(authoritative).expect("an authoritative event is selected");
    assert_eq!(chosen.id, newer_own.id);
    assert_eq!(
        servers_from_event(chosen),
        vec!["https://own-newer.example".to_string()]
    );
}

#[test]
fn authoritative_server_list_events_rejects_future_events() {
    let keys = Keys::generate();
    let now = Timestamp::now().as_secs();

    // More than the five-minute skew into the future: the timestamp has not
    // happened yet, so the event cannot be a settled server list.
    let future = at(&keys, &["https://future.example"], now + 10 * 60);
    let events = [future];

    assert!(
        authoritative_server_list_events(events.iter(), &keys.public_key()).is_empty(),
        "future-dated events must be rejected"
    );
}

// ---------------------------------------------------------------------------
// kind 1063 — NIP-94 blob locations
// ---------------------------------------------------------------------------

#[test]
fn blob_urls_from_events_collects_url_and_fallback_tags() {
    let keys = Keys::generate();
    let hash = "a".repeat(64);
    let event = file_metadata_event(
        &keys,
        &hash,
        &[
            ("url", "https://primary.example/blob.mp4"),
            ("fallback", "https://mirror.example/blob.mp4"),
        ],
    );

    let events = [event];
    assert_eq!(
        blob_urls_from_events(events.iter(), &hash),
        vec![
            "https://primary.example/blob.mp4".to_string(),
            "https://mirror.example/blob.mp4".to_string(),
        ]
    );
}

#[test]
fn blob_urls_from_events_deduplicates_across_events() {
    let keys = Keys::generate();
    let hash = "b".repeat(64);
    let shared = "https://shared.example/blob.mp4";

    let first = file_metadata_event(&keys, &hash, &[("url", shared)]);
    let second = file_metadata_event(
        &keys,
        &hash,
        &[("url", shared), ("fallback", "https://other.example/b.mp4")],
    );

    let events = [first, second];
    assert_eq!(
        blob_urls_from_events(events.iter(), &hash),
        vec![
            shared.to_string(),
            "https://other.example/b.mp4".to_string(),
        ],
        "duplicate URLs must appear once, in first-seen order"
    );
}

#[test]
fn blob_urls_from_events_filters_ssrf_targets() {
    let keys = Keys::generate();
    let hash = "c".repeat(64);
    // A hostile relay can publish anything; private targets must never survive.
    let event = file_metadata_event(
        &keys,
        &hash,
        &[
            ("url", "http://127.0.0.1:8080/blob.mp4"),
            ("url", "http://localhost/blob.mp4"),
            ("url", "http://169.254.169.254/latest/meta-data"),
            ("url", "http://10.0.0.5/blob.mp4"),
            ("url", "http://192.168.1.10/blob.mp4"),
            ("fallback", "file:///etc/passwd"),
            ("url", "https://public.example/blob.mp4"),
        ],
    );

    let events = [event];
    assert_eq!(
        blob_urls_from_events(events.iter(), &hash),
        vec!["https://public.example/blob.mp4".to_string()],
        "only the public HTTPS URL may survive filtering"
    );
}

#[test]
fn blob_urls_from_events_ignores_non_location_tags() {
    let keys = Keys::generate();
    let hash = "d".repeat(64);
    let event = file_metadata_event(
        &keys,
        &hash,
        &[
            ("m", "video/mp4"),
            ("size", "12345"),
            ("url", "https://kept.example/blob.mp4"),
        ],
    );

    let events = [event];
    assert_eq!(
        blob_urls_from_events(events.iter(), &hash),
        vec!["https://kept.example/blob.mp4".to_string()]
    );
}

#[test]
fn blob_urls_from_events_returns_empty_for_no_events() {
    let events: Vec<Event> = Vec::new();
    assert!(blob_urls_from_events(events.iter(), &"a".repeat(64)).is_empty());
}

#[test]
fn blob_urls_from_events_requires_matching_x_tag() {
    let keys = Keys::generate();
    let wanted_hash = "a".repeat(64);
    let other_hash = "b".repeat(64);

    // An event attesting to a *different* hash must not resolve the wanted one:
    // a hostile relay can publish kind-1063 events for arbitrary hashes.
    let mismatched = file_metadata_event(
        &keys,
        &other_hash,
        &[("url", "https://wrong-hash.example/blob.mp4")],
    );
    // A kind-1063 event with no `x` tag at all must not either.
    let no_x_tag = EventBuilder::new(Kind::from(KIND_FILE_METADATA), "")
        .tags([Tag::parse(["url", "https://no-x-tag.example/blob.mp4"]).unwrap()])
        .finalize(&keys)
        .expect("sign file metadata event without x tag");
    let matching = file_metadata_event(
        &keys,
        &wanted_hash,
        &[("url", "https://correct.example/blob.mp4")],
    );

    let events = [mismatched, no_x_tag, matching];
    assert_eq!(
        blob_urls_from_events(events.iter(), &wanted_hash),
        vec!["https://correct.example/blob.mp4".to_string()],
        "only events attesting to the requested hash may contribute URLs"
    );
}

// ---------------------------------------------------------------------------
// End-to-end: signed events feeding the server-list combination logic
// ---------------------------------------------------------------------------

#[test]
fn author_servers_from_signed_event_feed_combine_server_lists() {
    let keys = Keys::generate();
    let event = server_list_event(
        &keys,
        &["cdn.author.example", "https://second.author.example"],
    );
    let author_servers = servers_from_event(&event);

    let combined = combine_server_lists(
        Some(&["https://hint.example".to_string()]),
        Some(&author_servers),
        &["https://fallback.example".to_string()],
        4,
    );

    // Priority: explicit hints, then author servers, then fallbacks.
    assert_eq!(
        combined,
        vec![
            "https://hint.example".to_string(),
            "https://cdn.author.example".to_string(),
            "https://second.author.example".to_string(),
            "https://fallback.example".to_string(),
        ]
    );
}

#[test]
fn blossom_url_from_nip94_event_round_trips_to_hash_and_extension() {
    let keys = Keys::generate();
    let hash = "e".repeat(64);
    let url = format!("https://cdn.example.com/{hash}.mp4");
    let event = file_metadata_event(&keys, &hash, &[("url", &url)]);

    let events = [event];
    let discovered = blob_urls_from_events(events.iter(), &hash);
    assert_eq!(discovered, vec![url.clone()]);

    // The discovered URL must be parseable back into the hash we searched for.
    let (parsed_hash, ext) = extract_blossom_hash(&discovered[0]).expect("hash extracted");
    assert_eq!(parsed_hash, hash);
    assert_eq!(ext, "mp4");
}

#[test]
fn signed_event_pubkey_round_trips_through_normalize_and_parse_helpers() {
    let keys = Keys::generate();
    let event = server_list_event(&keys, &["example.com/"]);

    // The event's author must round-trip through our pubkey parser.
    assert_eq!(
        parse_pubkey(&event.pubkey.to_hex()).unwrap(),
        keys.public_key()
    );
    // And the tag value must survive normalization consistently.
    assert_eq!(normalize_server_url("example.com/"), "https://example.com");
    assert_eq!(
        servers_from_event(&event),
        vec!["https://example.com".to_string()]
    );
}

#[test]
fn parse_blossom_filename_matches_hash_in_signed_event() {
    let keys = Keys::generate();
    let hash = "f".repeat(64);
    let event = file_metadata_event(&keys, &hash, &[("url", "https://cdn.example.com/x.mp4")]);

    // The `x` tag on the event is the hash clients address the blob by.
    let x_tag = event
        .tags
        .iter()
        .map(|tag| tag.clone().to_vec())
        .find(|parts| parts.first().map(String::as_str) == Some("x"))
        .expect("x tag present");

    let filename = format!("{}.mp4", x_tag[1]);
    assert_eq!(
        parse_blossom_filename(&filename),
        Some((hash.as_str(), Some("mp4")))
    );
}

// ---------------------------------------------------------------------------
// Hard caps: xs= hint count and total blob candidate fan-out
// ---------------------------------------------------------------------------

#[test]
fn combine_server_lists_truncates_excess_xs_hints() {
    // `xs=` comes from the query string: an attacker can send any number of
    // hints, and each honored hint becomes a candidate host we may contact.
    // Surplus hints must be dropped, not rejected, so the API stays compatible.
    let hints = (0..10)
        .map(|index| format!("hint{index}.example"))
        .collect::<Vec<_>>();

    let combined = combine_server_lists(Some(&hints), None, &[], 4);

    assert_eq!(
        combined,
        vec![
            "https://hint0.example".to_string(),
            "https://hint1.example".to_string(),
            "https://hint2.example".to_string(),
            "https://hint3.example".to_string(),
        ],
        "only the first max_server_hints hints may be honored"
    );
}

/// Spawn a local HTTP server that answers every request with 404 and counts
/// how many times it was hit.
async fn spawn_not_found_server() -> (
    std::net::SocketAddr,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = requests.clone();
    tokio::spawn(async move {
        let app = axum::Router::new().fallback(axum::routing::get(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                axum::http::StatusCode::NOT_FOUND
            }
        }));
        axum::serve(listener, app).await.unwrap();
    });
    (address, requests)
}

#[tokio::test]
async fn fetch_blob_truncates_candidate_list() {
    use rust_imgproxy::blossom::{fetch_blob, CandidateFailureCache};
    use std::time::{Duration, Instant};

    // Five reachable (but 404-ing) hosts; the cap must limit the fan-out even
    // though hints, author servers and discovery supplied more candidates.
    let mut addresses = Vec::new();
    let mut counters = Vec::new();
    for _ in 0..5 {
        let (address, requests) = spawn_not_found_server().await;
        addresses.push(address);
        counters.push(requests);
    }

    rust_imgproxy::init_crypto_provider();
    let mut client = reqwest::Client::builder();
    let mut servers = Vec::new();
    for (index, address) in addresses.iter().enumerate() {
        let host = format!("cdn{index}.example");
        client = client.resolve(&host, *address);
        servers.push(format!("http://{host}:{}", address.port()));
    }

    let cache = CandidateFailureCache::new(
        Duration::from_secs(60),
        Duration::from_secs(60),
        Duration::from_secs(60),
    );
    let result = fetch_blob(
        &client.build().unwrap(),
        &cache,
        &servers,
        &[],
        &"a".repeat(64),
        Some("bin"),
        Instant::now() + Duration::from_secs(2),
        1024 * 1024,
        2, // max_blob_candidates
        Duration::from_secs(5),
    )
    .await;

    assert!(result.is_err(), "all truncated candidates fail");
    let hits = counters
        .iter()
        .map(|counter| counter.load(std::sync::atomic::Ordering::Relaxed))
        .sum::<usize>();
    assert_eq!(
        hits, 2,
        "only max_blob_candidates hosts may be contacted, got {hits} upstream requests"
    );
}

// ---------------------------------------------------------------------------
// Live relay connectivity (network; opt-in)
// ---------------------------------------------------------------------------

/// Proves the installed `nostr-sdk` can actually connect, subscribe and return
/// events from public relays. Guards against SDK upgrades that compile but
/// break the wire protocol.
#[tokio::test]
#[ignore = "requires public relay network access"]
async fn live_relays_return_events_for_a_broad_filter() {
    let client = Client::default();
    for relay in ["wss://nos.lol", "wss://relay.primal.net"] {
        client.add_relay(relay).await.expect("add relay");
    }
    client.connect().await;

    let events = fetch_events_from(
        &client,
        &["wss://nos.lol", "wss://relay.primal.net"],
        Filter::new().kind(Kind::TextNote).limit(5),
        std::time::Duration::from_secs(15),
    )
    .await
    .expect("relay query succeeds");

    assert!(
        !events.is_empty(),
        "expected at least one text note from public relays"
    );
    // Every returned event must carry a valid signature.
    for event in events.iter() {
        assert!(event.verify().is_ok(), "relay returned an invalid event");
    }
}

/// Exercises the real kind-10063 discovery path end to end against public
/// relays, using a pubkey known to publish a Blossom server list.
#[tokio::test]
#[ignore = "requires public relay network access"]
async fn live_relays_resolve_a_kind_10063_server_list() {
    let client = Client::default();
    for relay in ["wss://nos.lol", "wss://purplepag.es"] {
        client.add_relay(relay).await.expect("add relay");
    }
    client.connect().await;

    let events = fetch_events_from(
        &client,
        &["wss://nos.lol", "wss://purplepag.es"],
        Filter::new().kind(Kind::from(KIND_SERVER_LIST)).limit(10),
        std::time::Duration::from_secs(15),
    )
    .await
    .expect("relay query succeeds");

    if events.is_empty() {
        eprintln!("no kind-10063 events available right now; skipping assertions");
        return;
    }

    let newest = best_event(events.iter()).expect("newest event");
    assert_eq!(newest.kind, Kind::from(KIND_SERVER_LIST));

    // Whatever the relays return, parsing must not panic and must yield
    // normalized absolute URLs.
    for server in servers_from_event(newest) {
        assert!(
            server.starts_with("http://") || server.starts_with("https://"),
            "normalized server must be absolute, got {server:?}"
        );
        assert!(!server.ends_with('/'), "trailing slash must be stripped");
    }
}

/// Drives the **real** production path: `BlossomState` wires up its own client,
/// connects to the seed relays and resolves a live author's BUD-03 server list.
///
/// This is the regression guard for the two failures that a compile-only
/// nostr-sdk upgrade leaves behind: relays that are registered but never
/// connected, and a rustls provider that is missing or ambiguous.
#[tokio::test]
#[ignore = "requires public relay network access"]
async fn live_blossom_state_resolves_a_real_author_server_list() {
    rust_imgproxy::init_crypto_provider();

    // Discover an author that actually publishes a kind-10063 list, so the
    // test does not depend on one hard-coded pubkey staying alive.
    let probe = Client::default();
    for relay in ["wss://nos.lol", "wss://purplepag.es"] {
        probe.add_relay(relay).await.expect("add relay");
    }
    probe.connect().await;

    let seed_events = fetch_events_from(
        &probe,
        &["wss://nos.lol", "wss://purplepag.es"],
        Filter::new().kind(Kind::from(KIND_SERVER_LIST)).limit(25),
        std::time::Duration::from_secs(15),
    )
    .await
    .expect("probe query succeeds");

    // Pick an author whose list actually parses into at least one server.
    let Some(author) = seed_events
        .iter()
        .find(|event| !servers_from_event(event).is_empty())
        .map(|event| event.pubkey)
    else {
        eprintln!("no usable kind-10063 event on the network right now; skipping");
        return;
    };

    let state = rust_imgproxy::blossom::BlossomState::new(
        std::time::Duration::from_secs(60),
        std::time::Duration::from_secs(60),
        std::time::Duration::from_secs(60),
        std::time::Duration::from_secs(10),
        rust_imgproxy::blossom::CandidateFailureCache::new(
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(60),
        ),
    )
    .await;

    let servers = state
        .get_author_servers(&author.to_hex())
        .await
        .expect("author server lookup succeeds against live relays");

    assert!(
        !servers.is_empty(),
        "expected a non-empty server list for author {}",
        author.to_hex()
    );
    for server in &servers {
        assert!(
            server.starts_with("http://") || server.starts_with("https://"),
            "normalized server must be absolute, got {server:?}"
        );
    }

    // A second call must be served from cache and agree with the first.
    let cached = state
        .get_author_servers(&author.to_hex())
        .await
        .expect("cached lookup succeeds");
    assert_eq!(cached, servers, "cached lookup must match the live result");
}
