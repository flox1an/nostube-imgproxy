use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::{
    collections::HashMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::error::SvcError;

type HmacSha256 = Hmac<Sha256>;

/// Versioned capability URL verifier. Keys are intentionally opaque to callers
/// and are never formatted with their secret material.
#[derive(Clone, Default)]
pub struct UrlSigningKeys {
    keys: HashMap<String, Vec<u8>>,
    active_key_id: Option<String>,
}

impl std::fmt::Debug for UrlSigningKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UrlSigningKeys")
            .field("key_count", &self.keys.len())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigningConfigError {
    EmptyKeyId,
    InvalidKeyId(String),
    DuplicateKeyId(String),
    InvalidSecretEncoding(String),
    ShortSecret(String),
}

impl std::fmt::Display for SigningConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyKeyId => f.write_str("URL signing key id cannot be empty"),
            Self::InvalidKeyId(key_id) => write!(f, "invalid URL signing key id: {key_id}"),
            Self::DuplicateKeyId(key_id) => write!(f, "duplicate URL signing key id: {key_id}"),
            Self::InvalidSecretEncoding(key_id) => {
                write!(
                    f,
                    "URL signing secret for {key_id} is not unpadded base64url"
                )
            }
            Self::ShortSecret(key_id) => {
                write!(
                    f,
                    "URL signing secret for {key_id} must be at least 32 bytes"
                )
            }
        }
    }
}

impl std::error::Error for SigningConfigError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureFailure {
    Invalid,
    MissingExpiry,
    MalformedExpiry,
    Expired,
}

impl SignatureFailure {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::MissingExpiry => "missing_expiry",
            Self::MalformedExpiry => "malformed_expiry",
            Self::Expired => "expired",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedUrl {
    pub expires_at: Option<SystemTime>,
}

/// A v1 capability signature minted with the active signing key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedUrl {
    pub key_id: String,
    pub signature: String,
}

impl UrlSigningKeys {
    /// Parse `key-id:base64url-secret` pairs. Multiple keys permit a clean
    /// signer rotation; a URL selects its key with the `key-id` path segment.
    pub fn parse(value: &str) -> Result<Self, SigningConfigError> {
        let mut keys = HashMap::new();
        let mut active_key_id = None;
        for entry in value.split(',').filter(|entry| !entry.trim().is_empty()) {
            let (key_id, encoded_secret) = entry
                .trim()
                .split_once(':')
                .ok_or_else(|| SigningConfigError::InvalidKeyId(entry.trim().to_owned()))?;
            validate_key_id(key_id)?;
            let secret = URL_SAFE_NO_PAD
                .decode(encoded_secret)
                .map_err(|_| SigningConfigError::InvalidSecretEncoding(key_id.to_owned()))?;
            if secret.len() < 32 {
                return Err(SigningConfigError::ShortSecret(key_id.to_owned()));
            }
            if keys.insert(key_id.to_owned(), secret).is_some() {
                return Err(SigningConfigError::DuplicateKeyId(key_id.to_owned()));
            }
            if active_key_id.is_none() {
                active_key_id = Some(key_id.to_owned());
            }
        }
        Ok(Self {
            keys,
            active_key_id,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Sign with the first configured key. Configuration order is therefore
    /// operationally significant: prepend a rotated key before switching
    /// signers, while existing keys remain available for verification.
    pub fn sign_active(&self, path_and_query: &str) -> Option<SignedUrl> {
        let key_id = self.active_key_id.as_ref()?.clone();
        let key = self.keys.get(&key_id)?;
        Some(SignedUrl {
            signature: sign(key, &key_id, path_and_query),
            key_id,
        })
    }

    /// Verify a v1 capability URL. The signature covers the selected key id and
    /// the exact raw path-and-query after the signature prefix; no parsed or
    /// reordered representation may be substituted here.
    pub fn verify(
        &self,
        key_id: &str,
        signature: &str,
        path_and_query: &str,
        require_expiry: bool,
        now: SystemTime,
    ) -> Result<VerifiedUrl, SignatureFailure> {
        let key = self.keys.get(key_id).ok_or(SignatureFailure::Invalid)?;
        let tag = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| SignatureFailure::Invalid)?;
        if tag.len() != 32 {
            return Err(SignatureFailure::Invalid);
        }

        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts arbitrary key sizes");
        mac.update(signing_payload(key_id, path_and_query).as_bytes());
        mac.verify_slice(&tag)
            .map_err(|_| SignatureFailure::Invalid)?;

        let expires_at = parse_expiry(path_and_query)?;
        if require_expiry && expires_at.is_none() {
            return Err(SignatureFailure::MissingExpiry);
        }
        if expires_at.is_some_and(|expires_at| expires_at <= now) {
            return Err(SignatureFailure::Expired);
        }

        Ok(VerifiedUrl { expires_at })
    }

    #[cfg(test)]
    fn sign_for_test(&self, key_id: &str, path_and_query: &str) -> String {
        let key = self.keys.get(key_id).expect("test key id");
        sign(key, key_id, path_and_query)
    }
}

fn sign(key: &[u8], key_id: &str, path_and_query: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts arbitrary key sizes");
    mac.update(signing_payload(key_id, path_and_query).as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

fn validate_key_id(key_id: &str) -> Result<(), SigningConfigError> {
    if key_id.is_empty() {
        return Err(SigningConfigError::EmptyKeyId);
    }
    if key_id.len() > 32
        || !key_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(SigningConfigError::InvalidKeyId(key_id.to_owned()));
    }
    Ok(())
}

/// Stable HMAC message. `path_and_query` starts with `/img/` or `/thumb/` and
/// includes the raw query string if present.
pub fn signing_payload(key_id: &str, path_and_query: &str) -> String {
    format!("nostube-imgproxy-url-v1\n{key_id}\n{path_and_query}")
}

fn parse_expiry(path_and_query: &str) -> Result<Option<SystemTime>, SignatureFailure> {
    let Some((_, raw_query)) = path_and_query.split_once('?') else {
        return Ok(None);
    };
    let mut expiry = None;
    for (name, value) in url::form_urlencoded::parse(raw_query.as_bytes()) {
        if name != "exp" {
            continue;
        }
        if expiry.is_some() {
            return Err(SignatureFailure::MalformedExpiry);
        }
        let seconds = value
            .parse::<u64>()
            .map_err(|_| SignatureFailure::MalformedExpiry)?;
        let expires_at = UNIX_EPOCH
            .checked_add(Duration::from_secs(seconds))
            .ok_or(SignatureFailure::MalformedExpiry)?;
        expiry = Some(expires_at);
    }
    Ok(expiry)
}

pub fn signature_error(failure: SignatureFailure) -> SvcError {
    match failure {
        SignatureFailure::Expired => SvcError::Forbidden("signed URL expired"),
        SignatureFailure::MissingExpiry
        | SignatureFailure::MalformedExpiry
        | SignatureFailure::Invalid => SvcError::Forbidden("invalid signed URL"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
    const PATH: &str = "/thumb/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.webp?f=webp&rs=fit%3A480%3A480&exp=2000000000";

    fn keys() -> UrlSigningKeys {
        UrlSigningKeys::parse(&format!("nostube-2026-08:{SECRET}")).unwrap()
    }

    #[test]
    fn verify_accepts_exact_signed_url_before_expiry() {
        let keys = keys();
        let signature = keys.sign_for_test("nostube-2026-08", PATH);
        assert_eq!(signature, "Jtw-yDwCCeG3DoBuxro4IRV3ozlKL8NbfWdslncqiSQ");
        let verified = keys
            .verify(
                "nostube-2026-08",
                &signature,
                PATH,
                true,
                UNIX_EPOCH + Duration::from_secs(1_999_999_999),
            )
            .unwrap();
        assert_eq!(
            verified.expires_at,
            Some(UNIX_EPOCH + Duration::from_secs(2_000_000_000))
        );
    }

    #[test]
    fn verify_rejects_tampered_query_and_expired_url() {
        let keys = keys();
        let signature = keys.sign_for_test("nostube-2026-08", PATH);
        assert_eq!(
            keys.verify(
                "nostube-2026-08",
                &signature,
                "/thumb/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.webp?f=jpeg&rs=fit%3A480%3A480&exp=2000000000",
                true,
                UNIX_EPOCH,
            ),
            Err(SignatureFailure::Invalid)
        );
        assert_eq!(
            keys.verify(
                "nostube-2026-08",
                &signature,
                PATH,
                true,
                UNIX_EPOCH + Duration::from_secs(2_000_000_000),
            ),
            Err(SignatureFailure::Expired)
        );
    }

    #[test]
    fn parse_rejects_short_or_duplicate_keys() {
        assert!(matches!(
            UrlSigningKeys::parse("key:AA"),
            Err(SigningConfigError::ShortSecret(_))
        ));
        assert!(matches!(
            UrlSigningKeys::parse(&format!("key:{SECRET},key:{SECRET}")),
            Err(SigningConfigError::DuplicateKeyId(_))
        ));
    }
}
