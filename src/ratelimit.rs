//! Bounded, in-memory per-IP rate limiting.
//!
//! Every limiter here is process-local: a multi-replica deployment must move
//! this state to shared TTL-capable infrastructure before relying on
//! cluster-wide budgets, exactly like the cache and singleflight state.

use parking_lot::Mutex;
use std::{
    collections::HashMap,
    net::IpAddr,
    time::{Duration, Instant},
};

use crate::error::SvcError;

/// Cap on distinct IPs tracked at once. Bounds memory under a many-source-IP
/// attack; once full, unseen IPs are rejected rather than evicting a tracked
/// IP early and letting it burst past its budget.
const MAX_TRACKED_IPS: usize = 10_000;

const WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone, Copy)]
struct Window {
    started: Instant,
    used: u32,
}

/// Fixed-window per-IP budget. One minute wide; not sliding, so a burst can
/// land just inside two adjacent windows, which is an acceptable trade for
/// O(1) bookkeeping.
pub struct IpRateLimiter {
    windows: Mutex<HashMap<IpAddr, Window>>,
    limit_per_min: u32,
}

impl IpRateLimiter {
    pub fn new(limit_per_min: u32) -> Self {
        Self {
            windows: Mutex::new(HashMap::new()),
            limit_per_min: limit_per_min.max(1),
        }
    }

    /// Admit `cost` units for `ip`, or reject with [`SvcError::RateLimited`]
    /// once the current minute's budget for that IP is exhausted.
    pub fn admit(&self, ip: IpAddr, cost: u32) -> Result<(), SvcError> {
        let now = Instant::now();
        let mut windows = self.windows.lock();
        windows.retain(|_, window| now.duration_since(window.started) < WINDOW);

        let can_consume = windows
            .get(&ip)
            .map(|window| window.used.saturating_add(cost) <= self.limit_per_min)
            .unwrap_or(cost <= self.limit_per_min);
        if !can_consume {
            return Err(SvcError::RateLimited);
        }
        if windows.len() >= MAX_TRACKED_IPS && !windows.contains_key(&ip) {
            return Err(SvcError::RateLimited);
        }

        let window = windows.entry(ip).or_insert(Window {
            started: now,
            used: 0,
        });
        window.used = window.used.saturating_add(cost);
        Ok(())
    }
}

/// Three-tier per-IP budget for the image/thumbnail serving routes.
///
/// A cache hit is cheap to serve, so `requests` is generous and covers every
/// request regardless of outcome. Producing a fresh derivative is not: a
/// cache miss additionally spends from `image_generations`, and a video
/// thumbnail — which drives an FFmpeg process — spends from the much
/// stricter `video_generations` instead. The tiers are independent budgets,
/// not a hierarchy: exhausting one does not touch the others.
pub struct MediaRateLimiters {
    requests: IpRateLimiter,
    image_generations: IpRateLimiter,
    video_generations: IpRateLimiter,
}

impl MediaRateLimiters {
    pub fn new(
        requests_per_min: u32,
        image_generations_per_min: u32,
        video_generations_per_min: u32,
    ) -> Self {
        Self {
            requests: IpRateLimiter::new(requests_per_min),
            image_generations: IpRateLimiter::new(image_generations_per_min),
            video_generations: IpRateLimiter::new(video_generations_per_min),
        }
    }

    /// Charge the general per-request budget. Call once per request, before
    /// the cache lookup.
    pub fn admit_request(&self, ip: IpAddr) -> Result<(), SvcError> {
        self.requests.admit(ip, 1)
    }

    /// Charge the generation budget for a cache miss. Call only once the
    /// request is known to require fresh decode/resize/encode or FFmpeg
    /// work, before that work starts.
    pub fn admit_generation(&self, ip: IpAddr, is_video: bool) -> Result<(), SvcError> {
        if is_video {
            self.video_generations.admit(ip, 1)
        } else {
            self.image_generations.admit(ip, 1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admits_up_to_the_configured_budget_then_rejects() {
        let limiter = IpRateLimiter::new(3);
        let ip: IpAddr = "203.0.113.10".parse().unwrap();
        limiter.admit(ip, 2).unwrap();
        limiter.admit(ip, 1).unwrap();
        assert!(matches!(limiter.admit(ip, 1), Err(SvcError::RateLimited)));
    }

    #[test]
    fn tracks_each_ip_independently() {
        let limiter = IpRateLimiter::new(1);
        let a: IpAddr = "203.0.113.10".parse().unwrap();
        let b: IpAddr = "198.51.100.20".parse().unwrap();
        limiter.admit(a, 1).unwrap();
        assert!(limiter.admit(b, 1).is_ok());
    }

    #[test]
    fn rejects_unseen_ips_once_the_tracked_set_is_full() {
        let limiter = IpRateLimiter::new(100);
        for i in 0..MAX_TRACKED_IPS {
            let ip = IpAddr::from([0, 0, (i >> 8) as u8, (i & 0xFF) as u8]);
            limiter.admit(ip, 1).unwrap();
        }
        let overflow: IpAddr = "203.0.113.10".parse().unwrap();
        assert!(matches!(
            limiter.admit(overflow, 1),
            Err(SvcError::RateLimited)
        ));
    }

    #[test]
    fn media_tiers_admit_generation_routes_video_and_image_to_separate_budgets() {
        let limiters = MediaRateLimiters::new(100, 1, 1);
        let ip: IpAddr = "203.0.113.10".parse().unwrap();

        limiters.admit_generation(ip, false).unwrap();
        assert!(matches!(
            limiters.admit_generation(ip, false),
            Err(SvcError::RateLimited)
        ));
        // The video budget is untouched by the exhausted image budget.
        limiters.admit_generation(ip, true).unwrap();
        assert!(matches!(
            limiters.admit_generation(ip, true),
            Err(SvcError::RateLimited)
        ));
    }

    #[test]
    fn media_tiers_request_budget_is_independent_of_generation_budgets() {
        let limiters = MediaRateLimiters::new(1, 100, 100);
        let ip: IpAddr = "203.0.113.10".parse().unwrap();

        limiters.admit_request(ip).unwrap();
        assert!(matches!(
            limiters.admit_request(ip),
            Err(SvcError::RateLimited)
        ));
        // Generation budgets remain open even though the general per-request
        // budget for this IP is exhausted.
        assert!(limiters.admit_generation(ip, false).is_ok());
        assert!(limiters.admit_generation(ip, true).is_ok());
    }
}
