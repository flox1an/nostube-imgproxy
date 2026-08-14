//! Bounded off-runtime execution for CPU-bound image work.
//!
//! Decoding, resizing and encoding are synchronous and may pin large source
//! buffers. `CpuPool` caps both running jobs and admitted waiters so overload is
//! shed before those buffers accumulate in the heap.

use std::sync::Arc;

use tokio::sync::Semaphore;

use crate::error::SvcError;

#[derive(Clone)]
pub struct CpuPool {
    permits: Arc<Semaphore>,
    admission: Arc<Semaphore>,
}

impl CpuPool {
    /// `concurrency` tracks cores. `queue_depth` is the maximum number of
    /// additional jobs allowed to wait for one; running jobs have their own
    /// admission slots and therefore do not consume that queue budget.
    pub fn new(concurrency: usize, queue_depth: usize) -> Self {
        let concurrency = concurrency.max(1);
        Self {
            permits: Arc::new(Semaphore::new(concurrency)),
            admission: Arc::new(Semaphore::new(concurrency.saturating_add(queue_depth))),
        }
    }

    /// Run `job` on a blocking thread, holding capacity for its whole duration.
    /// A full admission queue is rejected immediately; waiting here would pin an
    /// already-downloaded original per request and turn traffic into heap DoS.
    pub async fn run<T, F>(&self, job: F) -> Result<T, SvcError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let admission = self
            .admission
            .clone()
            .try_acquire_owned()
            .map_err(|_| SvcError::Overloaded)?;
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| SvcError::InternalError("cpu pool closed".into()))?;

        tokio::task::spawn_blocking(move || {
            let result = job();
            drop(permit);
            drop(admission);
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
        let pool = CpuPool::new(2, 2);
        assert_eq!(pool.run(|| 21 * 2).await.unwrap(), 42);
    }

    #[tokio::test]
    async fn run_reports_a_panicking_job_without_aborting() {
        let pool = CpuPool::new(1, 1);
        let error = pool
            .run(|| panic!("decoder exploded"))
            .await
            .expect_err("panic must surface as an error");
        assert!(matches!(error, SvcError::InternalError(_)));
        assert_eq!(pool.run(|| 1).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn run_never_exceeds_the_configured_concurrency() {
        let pool = CpuPool::new(2, 16);
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

    #[tokio::test]
    async fn run_rejects_when_cpu_queue_is_full() {
        let pool = CpuPool::new(1, 0);
        let first = {
            let pool = pool.clone();
            tokio::spawn(async move {
                pool.run(|| std::thread::sleep(std::time::Duration::from_millis(100)))
                    .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(matches!(pool.run(|| ()).await, Err(SvcError::Overloaded)));
        first.await.unwrap().unwrap();
    }
}
