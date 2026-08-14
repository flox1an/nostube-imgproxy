use base64::{engine::general_purpose::STANDARD, Engine as _};
use nostr_sdk::prelude::{Event, Kind, Timestamp};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    net::IpAddr,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::{
    config::MintConfig, error::SvcError, signing::UrlSigningKeys, thumbnail::is_video_url,
};

const MAX_AUTHORIZATION_BYTES: usize = 16 * 1024;
const MAX_ADMISSION_ENTRIES: usize = 10_000;
const NIP98_MAX_FUTURE_SECS: u64 = 30;
const NIP98_MAX_PAST_SECS: u64 = 60;

#[derive(Clone, Default)]
pub struct MintState {
    admission: Arc<Mutex<AdmissionState>>,
}

#[derive(Default)]
struct AdmissionState {
    used_events: HashMap<String, Instant>,
    ip_windows: HashMap<IpAddr, RateWindow>,
    pubkey_windows: HashMap<String, RateWindow>,
}

#[derive(Clone, Copy)]
struct RateWindow {
    started: Instant,
    used: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MintRequest {
    pub preset: MintPreset,
    pub items: Vec<MintItem>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MintItem {
    /// Caller-chosen stable association key. It is echoed in the response so a
    /// batch can be reordered without losing the media-to-URL association.
    pub id: String,
    pub sha256: String,
    pub extension: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MintPreset {
    FeedPreviewV1,
    ProfileAvatarV1,
    EmbedCardV1,
}

impl MintPreset {
    fn directive_query(self) -> &'static str {
        match self {
            Self::FeedPreviewV1 => "f=webp&rs=fit%3A480%3A480&q=82",
            Self::ProfileAvatarV1 => "f=webp&rs=fill%3A160%3A160&q=85",
            Self::EmbedCardV1 => "f=webp&rs=fit%3A1200%3A630&q=82",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::FeedPreviewV1 => "feed-preview-v1",
            Self::ProfileAvatarV1 => "profile-avatar-v1",
            Self::EmbedCardV1 => "embed-card-v1",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct MintResponse {
    pub preset: &'static str,
    pub expires_at: u64,
    pub items: Vec<MintedItem>,
}

#[derive(Debug, Serialize)]
pub struct MintedItem {
    pub id: String,
    pub url: String,
}

impl MintState {
    /// Validate one NIP-98 authorization and atomically consume its replay and
    /// rate-limit budget. Caller supplies the exact raw JSON bytes, not a
    /// reserialized value, because NIP-98 binds the payload hash to those bytes.
    pub fn authorize(
        &self,
        authorization: &str,
        expected_url: &str,
        body: &[u8],
        peer_ip: IpAddr,
        cost: u32,
        cfg: &MintConfig,
    ) -> Result<(), SvcError> {
        let event = parse_nip98_event(authorization)?;
        validate_nip98_event(&event, expected_url, body)?;
        self.admit(event.id.to_hex(), event.pubkey.to_hex(), peer_ip, cost, cfg)
    }

    pub fn validate_request(request: &MintRequest, cfg: &MintConfig) -> Result<(), SvcError> {
        validate_request(request, cfg)
    }

    pub fn mint(
        &self,
        request: MintRequest,
        signing_keys: &UrlSigningKeys,
        cfg: &MintConfig,
    ) -> Result<MintResponse, SvcError> {
        validate_request(&request, cfg)?;
        let expires_at = expiry_bucket(SystemTime::now(), cfg.signed_url_ttl);
        let mut items = Vec::with_capacity(request.items.len());

        for item in request.items {
            let hash = item.sha256.to_ascii_lowercase();
            let extension = item
                .extension
                .map(|extension| extension.to_ascii_lowercase());
            let filename = match extension.as_deref() {
                Some(extension) => format!("{hash}.{extension}"),
                None => hash,
            };
            let path_and_query = format!(
                "/thumb/{filename}?{}&exp={expires_at}",
                request.preset.directive_query()
            );
            let signed = signing_keys
                .sign_active(&path_and_query)
                .ok_or_else(|| SvcError::InternalError("no active URL signing key".into()))?;
            let base_url = cfg
                .public_base_url
                .as_deref()
                .ok_or_else(|| SvcError::InternalError("mint public URL is unavailable".into()))?;
            items.push(MintedItem {
                id: item.id,
                url: format!(
                    "{base_url}/v1/{}/{signature}{path_and_query}",
                    signed.key_id,
                    signature = signed.signature,
                ),
            });
        }

        Ok(MintResponse {
            preset: request.preset.as_str(),
            expires_at,
            items,
        })
    }

    fn admit(
        &self,
        event_id: String,
        pubkey: String,
        peer_ip: IpAddr,
        cost: u32,
        cfg: &MintConfig,
    ) -> Result<(), SvcError> {
        let now = Instant::now();
        let mut state = self.admission.lock();
        state.used_events.retain(|_, expiry| *expiry > now);
        state
            .ip_windows
            .retain(|_, window| now.duration_since(window.started) < Duration::from_secs(60));
        state
            .pubkey_windows
            .retain(|_, window| now.duration_since(window.started) < Duration::from_secs(60));

        if state.used_events.contains_key(&event_id) {
            return Err(SvcError::Unauthorized);
        }
        if state.used_events.len() >= MAX_ADMISSION_ENTRIES {
            return Err(SvcError::Overloaded);
        }
        if !can_consume(
            state.ip_windows.get(&peer_ip),
            cost,
            cfg.rate_ip_items_per_min,
        ) || !can_consume(
            state.pubkey_windows.get(&pubkey),
            cost,
            cfg.rate_pubkey_items_per_min,
        ) {
            return Err(SvcError::RateLimited);
        }
        if state.ip_windows.len() >= MAX_ADMISSION_ENTRIES
            && !state.ip_windows.contains_key(&peer_ip)
        {
            return Err(SvcError::RateLimited);
        }
        if state.pubkey_windows.len() >= MAX_ADMISSION_ENTRIES
            && !state.pubkey_windows.contains_key(&pubkey)
        {
            return Err(SvcError::RateLimited);
        }

        state.used_events.insert(event_id, now + cfg.replay_ttl);
        consume(&mut state.ip_windows, peer_ip, cost, now);
        consume(&mut state.pubkey_windows, pubkey, cost, now);
        Ok(())
    }
}

fn parse_nip98_event(authorization: &str) -> Result<Event, SvcError> {
    let encoded = authorization
        .strip_prefix("Nostr ")
        .filter(|encoded| !encoded.is_empty() && encoded.len() <= MAX_AUTHORIZATION_BYTES)
        .ok_or(SvcError::Unauthorized)?;
    let decoded = STANDARD
        .decode(encoded)
        .map_err(|_| SvcError::Unauthorized)?;
    Event::from_json(decoded).map_err(|_| SvcError::Unauthorized)
}

fn validate_nip98_event(event: &Event, expected_url: &str, body: &[u8]) -> Result<(), SvcError> {
    if event.kind != Kind::HttpAuth || !event.content.is_empty() || event.verify().is_err() {
        return Err(SvcError::Unauthorized);
    }

    let now = Timestamp::now().as_secs();
    let created_at = event.created_at.as_secs();
    if created_at > now.saturating_add(NIP98_MAX_FUTURE_SECS)
        || now.saturating_sub(created_at) > NIP98_MAX_PAST_SECS
    {
        return Err(SvcError::Unauthorized);
    }

    let url = unique_tag_value(event, "u").ok_or(SvcError::Unauthorized)?;
    let method = unique_tag_value(event, "method").ok_or(SvcError::Unauthorized)?;
    let payload = unique_tag_value(event, "payload").ok_or(SvcError::Unauthorized)?;
    if url != expected_url || method != "POST" || payload.len() != 64 {
        return Err(SvcError::Unauthorized);
    }

    let body_hash = hex::encode(Sha256::digest(body));
    if payload != body_hash {
        return Err(SvcError::Unauthorized);
    }
    Ok(())
}

fn unique_tag_value<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    let mut values = event.tags.iter().filter_map(|tag| {
        let values = tag.as_slice();
        (values.len() == 2 && values[0] == name).then(|| values[1].as_str())
    });
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

fn validate_request(request: &MintRequest, cfg: &MintConfig) -> Result<(), SvcError> {
    if request.items.is_empty() || request.items.len() > cfg.max_batch_items {
        return Err(SvcError::BadRequest("invalid mint batch size"));
    }
    let mut ids = HashSet::with_capacity(request.items.len());
    for item in &request.items {
        if !valid_item_id(&item.id) || !ids.insert(&item.id) {
            return Err(SvcError::BadRequest("invalid or duplicate mint item id"));
        }
        if item.sha256.len() != 64 || !item.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(SvcError::BadRequest("invalid blob hash"));
        }
        if let Some(extension) = &item.extension {
            if !is_supported_extension(extension) {
                return Err(SvcError::BadRequest("unsupported blob extension"));
            }
        }
    }
    Ok(())
}

fn valid_item_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'-' | b'_' | b'.'))
}

fn is_supported_extension(extension: &str) -> bool {
    let extension = extension.to_ascii_lowercase();
    matches!(extension.as_str(), "jpg" | "jpeg" | "png" | "webp")
        || is_video_url(&format!("file.{extension}"))
}

fn expiry_bucket(now: SystemTime, ttl: Duration) -> u64 {
    let now_secs = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    let ttl_secs = ttl.as_secs();
    let requested = now_secs.saturating_add(ttl_secs);
    let buckets = requested / ttl_secs + u64::from(!requested.is_multiple_of(ttl_secs));
    buckets.saturating_mul(ttl_secs)
}

fn can_consume(window: Option<&RateWindow>, cost: u32, limit: u32) -> bool {
    window
        .map(|window| window.used.saturating_add(cost) <= limit)
        .unwrap_or(cost <= limit)
}

fn consume<K: std::cmp::Eq + std::hash::Hash>(
    windows: &mut HashMap<K, RateWindow>,
    key: K,
    cost: u32,
    now: Instant,
) {
    let window = windows.entry(key).or_insert(RateWindow {
        started: now,
        used: 0,
    });
    window.used = window.used.saturating_add(cost);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authorization(body: &[u8], url: &str, keys: &nostr_sdk::prelude::Keys) -> String {
        use nostr_sdk::prelude::{EventBuilder, FinalizeEvent, Tag};

        let payload = hex::encode(Sha256::digest(body));
        let event = EventBuilder::new(Kind::HttpAuth, "")
            .tags([
                Tag::parse(["u", url]).unwrap(),
                Tag::parse(["method", "POST"]).unwrap(),
                Tag::parse(["payload", &payload]).unwrap(),
            ])
            .finalize(keys)
            .unwrap();
        format!("Nostr {}", STANDARD.encode(event.as_json()))
    }

    #[test]
    fn nip98_authorization_binds_the_exact_body_and_rejects_replay() {
        let cfg = config();
        let keys = nostr_sdk::prelude::Keys::generate();
        let body = br#"{"preset":"feed-preview-v1","items":[]}"#;
        let auth = authorization(body, "https://img.example/v1/mint", &keys);
        let state = MintState::default();

        state
            .authorize(
                &auth,
                "https://img.example/v1/mint",
                body,
                "203.0.113.10".parse().unwrap(),
                1,
                &cfg,
            )
            .unwrap();
        assert!(matches!(
            state.authorize(
                &auth,
                "https://img.example/v1/mint",
                body,
                "203.0.113.10".parse().unwrap(),
                1,
                &cfg,
            ),
            Err(SvcError::Unauthorized)
        ));
        assert!(matches!(
            MintState::default().authorize(
                &auth,
                "https://img.example/v1/mint",
                br#"{"preset":"embed-card-v1","items":[]}"#,
                "203.0.113.10".parse().unwrap(),
                1,
                &cfg,
            ),
            Err(SvcError::Unauthorized)
        ));
    }

    #[test]
    fn admission_requires_both_ip_and_pubkey_budget() {
        let mut cfg = config();
        cfg.rate_ip_items_per_min = 2;
        cfg.rate_pubkey_items_per_min = 2;
        let state = MintState::default();
        let ip = "203.0.113.10".parse().unwrap();
        state
            .admit("event-1".into(), "pubkey-1".into(), ip, 2, &cfg)
            .unwrap();
        assert!(matches!(
            state.admit("event-2".into(), "pubkey-2".into(), ip, 1, &cfg),
            Err(SvcError::RateLimited)
        ));
    }

    #[test]
    fn mint_returns_verifiable_urls_with_the_callers_ids() {
        let keys =
            UrlSigningKeys::parse("nostube-2026-08:AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8")
                .unwrap();
        let response = MintState::default()
            .mint(
                MintRequest {
                    preset: MintPreset::FeedPreviewV1,
                    items: vec![MintItem {
                        id: "event:1:media:0".into(),
                        sha256: "a".repeat(64),
                        extension: Some("webp".into()),
                    }],
                },
                &keys,
                &config(),
            )
            .unwrap();

        assert_eq!(response.items[0].id, "event:1:media:0");
        let signed = response.items[0]
            .url
            .strip_prefix("https://img.example/v1/nostube-2026-08/")
            .unwrap();
        let (signature, path) = signed.split_once("/thumb").unwrap();
        keys.verify(
            "nostube-2026-08",
            signature,
            &format!("/thumb{path}"),
            true,
            SystemTime::now(),
        )
        .unwrap();
    }
    fn config() -> MintConfig {
        MintConfig {
            enabled: true,
            public_base_url: Some("https://img.example".into()),
            allowed_origins: vec!["https://nostube.example".into()],
            max_batch_items: 100,
            rate_ip_items_per_min: 10,
            rate_pubkey_items_per_min: 10,
            replay_ttl: Duration::from_secs(90),
            signed_url_ttl: Duration::from_secs(21_600),
        }
    }

    #[test]
    fn request_validation_preserves_batch_association_rules() {
        let request = MintRequest {
            preset: MintPreset::FeedPreviewV1,
            items: vec![
                MintItem {
                    id: "event:1:media:0".into(),
                    sha256: "a".repeat(64),
                    extension: Some("webp".into()),
                },
                MintItem {
                    id: "event:1:media:1".into(),
                    sha256: "b".repeat(64),
                    extension: Some("mp4".into()),
                },
            ],
        };
        assert!(validate_request(&request, &config()).is_ok());
    }

    #[test]
    fn request_validation_rejects_duplicate_ids_and_direct_urls() {
        let request = MintRequest {
            preset: MintPreset::FeedPreviewV1,
            items: vec![
                MintItem {
                    id: "same".into(),
                    sha256: "a".repeat(64),
                    extension: Some("webp".into()),
                },
                MintItem {
                    id: "same".into(),
                    sha256: "b".repeat(64),
                    extension: Some("https://example.com/a.jpg".into()),
                },
            ],
        };
        assert!(matches!(
            validate_request(&request, &config()),
            Err(SvcError::BadRequest(_))
        ));
    }

    #[test]
    fn expiry_bucket_is_stable_within_one_ttl_window() {
        assert_eq!(
            expiry_bucket(UNIX_EPOCH + Duration::from_secs(1), Duration::from_secs(60)),
            120
        );
        assert_eq!(
            expiry_bucket(
                UNIX_EPOCH + Duration::from_secs(59),
                Duration::from_secs(60)
            ),
            120
        );
    }
}
