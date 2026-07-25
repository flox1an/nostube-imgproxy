//! Bounded off-runtime execution for CPU-bound image work.
//!
//! Decoding, resizing and encoding are the only genuinely expensive things this
//! service does, and they are all synchronous. Running them inline on an Axum
//! handler blocks a Tokio worker for the whole job, so on a small edge node a
//! handful of concurrent AVIF encodes starve the reactor and `/health` stops
//! answering. Every such job goes through [`CpuPool`] instead: a semaphore caps
//! how many run at once, and `spawn_blocking` keeps them off the async workers.

use std::sync::Arc;

use tokio::sync::Semaphore;

use crate::error::SvcError;

#[derive(Clone)]
pub struct CpuPool {
    permits: Arc<Semaphore>,
}

impl CpuPool {
    /// `concurrency` should track available cores, not request concurrency.
    pub fn new(concurrency: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(concurrency.max(1))),
        }
    }

    /// Run `job` on a blocking thread, holding a permit for its whole duration.
    ///
    /// A panic inside `job` (a malformed image tripping a decoder bug) is
    /// reported as an internal error rather than aborting the process.
    pub async fn run<T, F>(&self, job: F) -> Result<T, SvcError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| SvcError::InternalError("cpu pool closed".into()))?;

        tokio::task::spawn_blocking(move || {
            let result = job();
            drop(permit);
            result
        })
        .await
        .map_err(|error| SvcError::InternalError(format!("cpu job failed: {error}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn run_returns_the_job_result() {
        let pool = CpuPool::new(2);
        assert_eq!(pool.run(|| 21 * 2).await.unwrap(), 42);
    }

    #[tokio::test]
    async fn run_reports_a_panicking_job_without_aborting() {
        let pool = CpuPool::new(1);
        let error = pool
            .run(|| panic!("decoder exploded"))
            .await
            .expect_err("panic must surface as an error");
        assert!(matches!(error, SvcError::InternalError(_)));

        // The permit must come back, or one bad image wedges the pool forever.
        assert_eq!(pool.run(|| 1).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn run_never_exceeds_the_configured_concurrency() {
        let pool = CpuPool::new(2);
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let jobs: Vec<_> = (0..16)
            .map(|_| {
                let pool = pool.clone();
                let live = live.clone();
                let peak = peak.clone();
                tokio::spawn(async move {
                    pool.run(move || {
                        let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        live.fetch_sub(1, Ordering::SeqCst);
                    })
                    .await
                    .unwrap()
                })
            })
            .collect();

        for job in jobs {
            job.await.unwrap();
        }
        assert!(peak.load(Ordering::SeqCst) <= 2, "concurrency cap breached");
    }
}
