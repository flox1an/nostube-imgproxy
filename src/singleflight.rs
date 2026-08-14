//! Request coalescing for cache misses.
//!
//! A detached driver owns each leader future after insertion. This is
//! intentional: otherwise a disconnect before the first poll leaves a `Shared`
//! future in the map forever, retaining its captured request data.

use std::{
    collections::HashMap,
    future::Future,
    panic::AssertUnwindSafe,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use bytes::Bytes;
use futures_util::future::{BoxFuture, FutureExt, Shared};
use http::StatusCode;
use parking_lot::Mutex;

use crate::error::SvcError;

#[derive(Clone, Debug)]
pub struct SharedError(StatusCode, Arc<str>);

impl From<SharedError> for SvcError {
    fn from(SharedError(status, message): SharedError) -> Self {
        SvcError::Rendered(status, message.to_string())
    }
}

type Flight = Shared<BoxFuture<'static, Result<Bytes, SharedError>>>;

#[derive(Clone)]
struct FlightEntry {
    generation: u64,
    flight: Flight,
}

#[derive(Debug)]
pub struct Outcome {
    pub bytes: Bytes,
    pub coalesced: bool,
}

pub struct SingleFlight {
    inflight: Mutex<HashMap<String, FlightEntry>>,
    next_generation: AtomicU64,
    max_entries: usize,
}

impl SingleFlight {
    /// `max_entries` caps unique cache misses in flight. A caller that cannot
    /// join an existing flight is shed rather than creating retained work.
    pub fn new(max_entries: usize) -> Self {
        Self {
            inflight: Mutex::new(HashMap::new()),
            next_generation: AtomicU64::new(0),
            max_entries: max_entries.max(1),
        }
    }

    pub async fn run<F, Fut>(self: &Arc<Self>, key: &str, work: F) -> Result<Outcome, SvcError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Bytes, SvcError>> + Send + 'static,
    {
        let (flight, coalesced) = {
            let mut inflight = self.inflight.lock();
            if let Some(existing) = inflight.get(key) {
                (existing.flight.clone(), true)
            } else {
                if inflight.len() >= self.max_entries {
                    return Err(SvcError::Overloaded);
                }
                let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
                let owner = Arc::clone(self);
                let owned_key = key.to_owned();
                let future = work();
                let flight: Flight = async move {
                    let result = AssertUnwindSafe(future).catch_unwind().await;
                    let result = match result {
                        Ok(result) => result,
                        Err(_) => Err(SvcError::InternalError(
                            "singleflight leader panicked".to_owned(),
                        )),
                    };
                    result.map_err(shared_error)
                }
                .boxed()
                .shared();
                inflight.insert(
                    owned_key.clone(),
                    FlightEntry {
                        generation,
                        flight: flight.clone(),
                    },
                );

                // The driver starts the future even if the leader disconnects
                // before awaiting it. On success, error, cancellation of every
                // HTTP caller, or caught panic it removes only its own map
                // generation, never a newer retry for the same key.
                let driver_flight = flight.clone();
                tokio::spawn(async move {
                    let _ = driver_flight.await;
                    let mut inflight = owner.inflight.lock();
                    if inflight
                        .get(&owned_key)
                        .is_some_and(|entry| entry.generation == generation)
                    {
                        inflight.remove(&owned_key);
                    }
                });
                (flight, false)
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

fn shared_error(error: SvcError) -> SharedError {
    let (status, message) = error.render();
    SharedError(status, Arc::from(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn run_executes_the_work_once_for_concurrent_callers() {
        let flight = Arc::new(SingleFlight::new(8));
        let runs = Arc::new(AtomicUsize::new(0));
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
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 8);
    }

    #[tokio::test]
    async fn run_forgets_a_key_once_the_flight_settles() {
        let flight = Arc::new(SingleFlight::new(1));
        flight
            .run("k", || async { Ok(Bytes::from_static(b"a")) })
            .await
            .unwrap();
        tokio::task::yield_now().await;
        assert_eq!(flight.len(), 0);
    }

    #[tokio::test]
    async fn run_removes_a_key_when_the_original_caller_is_dropped() {
        let flight = Arc::new(SingleFlight::new(1));
        let task = {
            let flight = Arc::clone(&flight);
            tokio::spawn(async move {
                flight
                    .run("k", || async {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        Ok(Bytes::from_static(b"a"))
                    })
                    .await
            })
        };
        task.abort();
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        assert_eq!(flight.len(), 0, "detached leader must clean up");
    }

    #[tokio::test]
    async fn run_recovers_after_a_panicking_leader() {
        let flight = Arc::new(SingleFlight::new(1));
        let error = flight
            .run("k", || async { panic!("decoder exploded") })
            .await
            .expect_err("panic must be rendered as an error");
        assert_eq!(error.render().0, StatusCode::INTERNAL_SERVER_ERROR);
        tokio::task::yield_now().await;
        assert_eq!(
            &flight
                .run("k", || async { Ok(Bytes::from_static(b"recovered")) })
                .await
                .unwrap()
                .bytes[..],
            b"recovered"
        );
    }

    #[tokio::test]
    async fn run_sheds_a_new_key_when_the_map_is_full() {
        let flight = Arc::new(SingleFlight::new(1));
        let pending = {
            let flight = Arc::clone(&flight);
            tokio::spawn(async move {
                flight
                    .run("first", || async {
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        Ok(Bytes::new())
                    })
                    .await
            })
        };
        tokio::task::yield_now().await;
        assert!(matches!(
            flight.run("second", || async { Ok(Bytes::new()) }).await,
            Err(SvcError::Overloaded)
        ));
        pending.abort();
    }
}
