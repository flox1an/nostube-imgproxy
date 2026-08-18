use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    net::IpAddr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    config::MintConfig, error::SvcError, ratelimit::IpRateLimiter, signing::UrlSigningKeys,
    thumbnail::is_video_url,
};

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

/// Anonymous, IP-rate-limited capability minting.
///
/// The endpoint is deliberately public: it only ever mints URLs for
/// already-public, hash-addressed Blossom media (anyone can fetch the same
/// bytes directly from a Blossom server), restricted to a handful of fixed
/// presets. Admission is therefore a flood guard, not an authorization
/// check, so it costs nothing to keep the endpoint usable by anonymous
/// browsers, embeds, and crawlers.
#[derive(Clone)]
pub struct MintState {
    admission: Arc<IpRateLimiter>,
}

impl MintState {
    pub fn new(rate_ip_items_per_min: u32) -> Self {
        Self {
            admission: Arc::new(IpRateLimiter::new(rate_ip_items_per_min)),
        }
    }

    pub fn validate_request(request: &MintRequest, cfg: &MintConfig) -> Result<(), SvcError> {
        validate_request(request, cfg)
    }

    /// Consume `cost` mint-item budget for `peer_ip`.
    pub fn admit(&self, peer_ip: IpAddr, cost: u32) -> Result<(), SvcError> {
        self.admission.admit(peer_ip, cost)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_enforces_the_per_ip_item_budget() {
        let state = MintState::new(2);
        let ip = "203.0.113.10".parse().unwrap();
        state.admit(ip, 2).unwrap();
        assert!(matches!(state.admit(ip, 1), Err(SvcError::RateLimited)));
    }

    #[test]
    fn admission_tracks_ips_independently() {
        let state = MintState::new(1);
        state.admit("203.0.113.10".parse().unwrap(), 1).unwrap();
        assert!(state.admit("198.51.100.20".parse().unwrap(), 1).is_ok());
    }

    #[test]
    fn mint_returns_verifiable_urls_with_the_callers_ids() {
        let keys =
            UrlSigningKeys::parse("nostube-2026-08:AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8")
                .unwrap();
        let response = MintState::new(10)
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
