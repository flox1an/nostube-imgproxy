//! Request coalescing for cache misses.
//!
//! Without this, N simultaneous requests for the same uncached derivative each
//! fetch the original upstream and each run the full decode/resize/encode
//! pipeline. On a cold edge node that turns one popular image into a thundering
//! herd against both the origin and the CPU. [`SingleFlight`] lets the first
//! caller for a key do the work and hands every concurrent caller the same
//! result.

use std::{collections::HashMap, future::Future, sync::Arc};

use bytes::Bytes;
use futures_util::future::{BoxFuture, FutureExt, Shared};
use http::StatusCode;
use parking_lot::Mutex;

use crate::error::SvcError;

/// A failure replayed to coalesced callers.
///
/// [`SvcError`] is not `Clone` (it wraps `reqwest`, `image` and `io` errors), so
/// the leader's error is rendered once and the rendering is what gets shared.
#[derive(Clone, Debug)]
pub struct SharedError(StatusCode, Arc<str>);

impl From<SharedError> for SvcError {
    fn from(SharedError(status, message): SharedError) -> Self {
        SvcError::Rendered(status, message.to_string())
    }
}

type Flight = Shared<BoxFuture<'static, Result<Bytes, SharedError>>>;

/// Outcome of a [`SingleFlight::run`] call.
#[derive(Debug)]
pub struct Outcome {
    pub bytes: Bytes,
    /// True when this caller attached to work already in progress.
    pub coalesced: bool,
}

#[derive(Default)]
pub struct SingleFlight {
    inflight: Mutex<HashMap<String, Flight>>,
}

impl SingleFlight {
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `work` for `key`, or join the run already underway for that key.
    ///
    /// `work` is only invoked by the leader. The entry is removed as soon as the
    /// leader finishes, so failures are never cached — the next request retries.
    pub async fn run<F, Fut>(self: &Arc<Self>, key: &str, work: F) -> Result<Outcome, SvcError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Bytes, SvcError>> + Send + 'static,
    {
        let (flight, coalesced) = {
            let mut inflight = self.inflight.lock();
            match inflight.get(key) {
                Some(existing) => (existing.clone(), true),
                None => {
                    let owner = Arc::clone(self);
                    let owned_key = key.to_string();
                    let future = work();
                    let flight: Flight = async move {
                        let result = future.await;
                        owner.inflight.lock().remove(&owned_key);
                        result.map_err(|error| {
                            let (status, message) = error.render();
                            SharedError(status, Arc::from(message.as_str()))
                        })
                    }
                    .boxed()
                    .shared();
                    inflight.insert(key.to_string(), flight.clone());
                    (flight, false)
                }
            }
        };

        let bytes = flight.await?;
        Ok(Outcome { bytes, coalesced })
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inflight.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn run_executes_the_work_once_for_concurrent_callers() {
        let flight = Arc::new(SingleFlight::new());
        let runs = Arc::new(AtomicUsize::new(0));

        // Keep the leader pending long enough for `join_all` to poll every
        // follower; each follower must then receive this same shared future.
        let callers: Vec<_> = (0..8)
            .map(|_| {
                let flight = Arc::clone(&flight);
                let runs = Arc::clone(&runs);
                async move {
                    flight
                        .run("same-key", || {
                            runs.fetch_add(1, Ordering::SeqCst);
                            async {
                                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                                Ok(Bytes::from_static(b"payload"))
                            }
                        })
                        .await
                }
            })
            .collect();

        let outcomes = futures_util::future::join_all(callers).await;
        let mut coalesced = 0;
        for outcome in outcomes {
            let outcome = outcome.expect("caller must succeed");
            assert_eq!(&outcome.bytes[..], b"payload");
            coalesced += usize::from(outcome.coalesced);
        }

        assert_eq!(runs.load(Ordering::SeqCst), 1, "work must run exactly once");
        assert_eq!(coalesced, 7, "every follower must be marked coalesced");
    }

    #[tokio::test]
    async fn run_replays_the_leader_failure_to_followers() {
        let flight = Arc::new(SingleFlight::new());
        let error = flight
            .run("bad", || async { Err(SvcError::UpstreamError(404)) })
            .await
            .expect_err("leader failure must surface");
        let (status, message) = error.render();
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(message, "Source image not found");
    }

    #[tokio::test]
    async fn run_forgets_a_key_once_the_flight_settles() {
        let flight = Arc::new(SingleFlight::new());

        flight
            .run("k", || async { Ok(Bytes::from_static(b"a")) })
            .await
            .unwrap();
        assert_eq!(flight.len(), 0, "settled flights must not be retained");

        // A failure must not be cached either: the next call re-runs the work.
        let _ = flight
            .run("k", || async { Err(SvcError::UpstreamError(502)) })
            .await;
        assert_eq!(flight.len(), 0);
    }

    #[tokio::test]
    async fn run_keeps_distinct_keys_independent() {
        let flight = Arc::new(SingleFlight::new());
        let a = flight
            .run("a", || async { Ok(Bytes::from_static(b"a")) })
            .await
            .unwrap();
        let b = flight
            .run("b", || async { Ok(Bytes::from_static(b"b")) })
            .await
            .unwrap();
        assert_eq!(&a.bytes[..], b"a");
        assert_eq!(&b.bytes[..], b"b");
        assert!(!a.coalesced && !b.coalesced);
    }
}
