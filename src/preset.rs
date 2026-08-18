//! Fixed output presets for the unsigned Blossom thumbnail route.
//!
//! A preset is the *only* server-authoritative mapping from a public name to
//! a derivative shape (format, quality, resize). Callers never supply
//! directives directly for this route: they pick one of a small, published
//! set of names. That closed shape is what makes the route safe to leave
//! unauthenticated — there is no open-ended value space to protect, so a
//! per-IP rate limit on the actual generation work is sufficient.

use crate::transform::{Directives, OutFmt, Resize, ResizeMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    FeedPreviewV1,
    ProfileAvatarV1,
    EmbedCardV1,
}

impl Preset {
    /// Parse a URL path segment into one of the fixed presets. Unknown
    /// segments fail closed: there is no default preset to fall back to.
    pub fn parse(segment: &str) -> Option<Self> {
        match segment {
            "feed-preview-v1" => Some(Self::FeedPreviewV1),
            "profile-avatar-v1" => Some(Self::ProfileAvatarV1),
            "embed-card-v1" => Some(Self::EmbedCardV1),
            _ => None,
        }
    }

    /// The fixed output directives for this preset.
    pub fn directives(self) -> Directives {
        let (mode, w, h, quality) = match self {
            Self::FeedPreviewV1 => (ResizeMode::Fit, 480, 480, 82),
            Self::ProfileAvatarV1 => (ResizeMode::Fill, 160, 160, 85),
            Self::EmbedCardV1 => (ResizeMode::Fit, 1200, 630, 82),
        };
        Directives {
            out_fmt: OutFmt::Webp,
            quality,
            resize: Resize { mode, w, h },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_every_documented_preset_name() {
        assert_eq!(
            Preset::parse("feed-preview-v1"),
            Some(Preset::FeedPreviewV1)
        );
        assert_eq!(
            Preset::parse("profile-avatar-v1"),
            Some(Preset::ProfileAvatarV1)
        );
        assert_eq!(Preset::parse("embed-card-v1"), Some(Preset::EmbedCardV1));
    }

    #[test]
    fn parse_rejects_unknown_or_free_form_names() {
        for bad in ["", "feed-preview-v2", "FEED-PREVIEW-V1", "custom:800:600"] {
            assert_eq!(Preset::parse(bad), None, "expected {bad:?} to be rejected");
        }
    }

    #[test]
    fn directives_match_the_documented_shape_for_each_preset() {
        let feed = Preset::FeedPreviewV1.directives();
        assert!(matches!(feed.out_fmt, OutFmt::Webp));
        assert_eq!(feed.quality, 82);
        assert!(matches!(feed.resize.mode, ResizeMode::Fit));
        assert_eq!((feed.resize.w, feed.resize.h), (480, 480));

        let avatar = Preset::ProfileAvatarV1.directives();
        assert_eq!(avatar.quality, 85);
        assert!(matches!(avatar.resize.mode, ResizeMode::Fill));
        assert_eq!((avatar.resize.w, avatar.resize.h), (160, 160));

        let embed = Preset::EmbedCardV1.directives();
        assert_eq!(embed.quality, 82);
        assert!(matches!(embed.resize.mode, ResizeMode::Fit));
        assert_eq!((embed.resize.w, embed.resize.h), (1200, 630));
    }
}
