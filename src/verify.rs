//! Opportunistic background verification of Blossom video blobs.
//!
//! A video thumbnail only needs a few seconds of footage near one keyframe,
//! which [`crate::thumbnail::extract_video_thumbnail`] range-probes cheaply.
//! Proving those bytes belong to the requested SHA-256, however, needs the
//! *whole* blob — SHA-256 is over the complete content, there is no partial
//! form. Putting that download on the request path would trade a bounded
//! probe for a full transfer on every miss, so it happens here instead:
//! after the response is already out, at most a few at a time, and only for
//! blobs that have proven worth caching.
//!
//! The payoff is written to the *original*-bytes cache, not to a derivative:
//! one verification then serves every preset and every resize of that blob.
//! Because a `thumb/` original is only ever written after a successful hash
//! check, its presence is exactly the proof `server::load_original` needs to
//! treat the blob as cacheable.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tracing::debug;

/// How long a blob's miss tally survives without a new miss. Misses have to
/// cluster to signal "worth caching"; a blob touched once an hour is an
/// archive read, not a hot thumbnail, and must never trigger a full download.
const MISS_WINDOW: Duration = Duration::from_secs(3600);

/// Ceiling on tracked blobs, so an attacker cycling distinct hashes cannot
/// grow this map without bound.
const MAX_TRACKED_BLOBS: usize = 10_000;

/// Wall-clock budget for one background verification. Generous compared with
/// a request timeout precisely because nothing is waiting on it.
pub const BACKGROUND_VERIFY_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug)]
struct BlobTally {
    misses: u32,
    /// Set once a verification has been handed out for this blob. Prevents a
    /// second download whether the first is still running, already succeeded,
    /// or failed — a failed verification retries only after `MISS_WINDOW`
    /// expiry drops the entry.
    settled: bool,
    expires_at: Instant,
}

/// Decides which unverified video blobs earn a background full-download, and
/// bounds how many run at once.
pub struct VideoVerifier {
    permits: Arc<Semaphore>,
    tallies: Mutex<HashMap<String, BlobTally>>,
    /// Range-probed misses a blob must accumulate before one verification is
    /// spawned. At the default of 2, a blob thumbnailed exactly once is never
    /// downloaded in full.
    miss_threshold: u32,
}

impl VideoVerifier {
    pub fn new(max_concurrent: usize, miss_threshold: u32) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max_concurrent.max(1))),
            tallies: Mutex::new(HashMap::new()),
            miss_threshold: miss_threshold.max(1),
        }
    }

    /// Record one unverified range-probe miss for `hash`.
    ///
    /// Returns a permit exactly once per blob per window, and only once the
    /// miss threshold is met and a concurrency slot is free. `None` means
    /// "do not verify now" — never an error, since verification is pure
    /// optimisation. The permit's lifetime is the caller's task: dropping it
    /// releases the slot.
    pub async fn claim(&self, hash: &str) -> Option<OwnedSemaphorePermit> {
        let now = Instant::now();
        let key = hash.to_ascii_lowercase();
        let mut tallies = self.tallies.lock().await;

        // Opportunistic sweep while the lock is already held.
        tallies.retain(|_, tally| tally.expires_at > now);

        let expires_at = now.checked_add(MISS_WINDOW)?;
        let tally = tallies.entry(key).or_insert_with(|| BlobTally {
            misses: 0,
            settled: false,
            expires_at,
        });
        tally.misses = tally.misses.saturating_add(1);
        tally.expires_at = expires_at;

        if tally.settled || tally.misses < self.miss_threshold {
            let misses = tally.misses;
            let settled = tally.settled;
            drop(tallies);
            debug!(misses, settled, "background video verification not due");
            return None;
        }

        // Claim before releasing the lock so two concurrent misses cannot both
        // pass the threshold check and spawn duplicate downloads.
        tally.settled = true;

        if tallies.len() > MAX_TRACKED_BLOBS {
            if let Some(oldest) = tallies
                .iter()
                .min_by_key(|(_, tally)| tally.expires_at)
                .map(|(key, _)| key.clone())
            {
                tallies.remove(&oldest);
            }
        }
        drop(tallies);

        // A busy slot means the node is already saturated with verifications;
        // skipping is correct, the next miss re-arms this blob below.
        match Arc::clone(&self.permits).try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                self.rearm(hash).await;
                debug!("background video verification skipped, no free slot");
                None
            }
        }
    }

    /// Undo a claim so a later miss can try again. Used when no concurrency
    /// slot was free, and by the caller when the spawn itself cannot proceed.
    pub async fn rearm(&self, hash: &str) {
        let key = hash.to_ascii_lowercase();
        if let Some(tally) = self.tallies.lock().await.get_mut(&key) {
            tally.settled = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn claim_withholds_a_permit_until_the_miss_threshold_is_met() {
        let verifier = VideoVerifier::new(4, 2);
        let hash = "a".repeat(64);

        assert!(
            verifier.claim(&hash).await.is_none(),
            "a blob seen once must never trigger a full download"
        );
        assert!(
            verifier.claim(&hash).await.is_some(),
            "the second miss meets the default threshold"
        );
    }

    #[tokio::test]
    async fn claim_hands_out_at_most_one_permit_per_blob() {
        let verifier = VideoVerifier::new(4, 1);
        let hash = "b".repeat(64);

        assert!(verifier.claim(&hash).await.is_some());
        assert!(
            verifier.claim(&hash).await.is_none(),
            "one verification per blob is enough; the result is shared via the cache"
        );
        assert!(verifier.claim(&hash).await.is_none());
    }

    #[tokio::test]
    async fn claim_tracks_each_blob_independently() {
        let verifier = VideoVerifier::new(4, 2);
        let first = "c".repeat(64);
        let second = "d".repeat(64);

        assert!(verifier.claim(&first).await.is_none());
        assert!(
            verifier.claim(&second).await.is_none(),
            "a different blob must not inherit the first blob's tally"
        );
        assert!(verifier.claim(&first).await.is_some());
        assert!(verifier.claim(&second).await.is_some());
    }

    #[tokio::test]
    async fn claim_is_case_insensitive_in_the_hash() {
        let verifier = VideoVerifier::new(4, 2);

        assert!(verifier.claim(&"E".repeat(64)).await.is_none());
        assert!(
            verifier.claim(&"e".repeat(64)).await.is_some(),
            "hash casing must not split one blob's tally in two"
        );
    }

    #[tokio::test]
    async fn claim_withholds_a_permit_when_every_slot_is_busy() {
        let verifier = VideoVerifier::new(1, 1);
        let held = verifier.claim(&"f".repeat(64)).await;
        assert!(held.is_some(), "the only slot is now taken");

        let hash = "0".repeat(64);
        assert!(
            verifier.claim(&hash).await.is_none(),
            "a saturated node must shed background work, not queue it"
        );

        // The blocked blob was re-armed, so it retries once a slot frees up.
        drop(held);
        assert!(
            verifier.claim(&hash).await.is_some(),
            "the next miss must retry after the slot is released"
        );
    }

    #[tokio::test]
    async fn rearm_allows_a_later_retry() {
        let verifier = VideoVerifier::new(4, 1);
        let hash = "1".repeat(64);

        let permit = verifier.claim(&hash).await;
        assert!(permit.is_some());
        assert!(verifier.claim(&hash).await.is_none());

        verifier.rearm(&hash).await;
        assert!(
            verifier.claim(&hash).await.is_some(),
            "an explicitly re-armed blob must be claimable again"
        );
    }
}
