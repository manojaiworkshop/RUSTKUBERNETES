//! `async.BoundedFrequencyRunner` (`pkg/util/async`): runs a function when
//! triggered, at most once per `min_interval` and at least once per `max_interval`.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

pub struct BoundedFrequencyRunner {
    notify: Arc<Notify>,
    pub min_interval: Duration,
    pub max_interval: Duration,
}

impl BoundedFrequencyRunner {
    pub fn new(min_interval: Duration, max_interval: Duration) -> Self {
        Self { notify: Arc::new(Notify::new()), min_interval, max_interval }
    }

    /// Handle to request a run (`Run()`); requests made during a run are coalesced.
    pub fn trigger(&self) -> Arc<Notify> {
        self.notify.clone()
    }

    /// Runs `f` immediately, then on every trigger (rate-limited) or after `max_interval`.
    pub async fn run<F, Fut>(&self, mut f: F)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = ()>,
    {
        loop {
            // Requests that arrived while waiting out `min_interval` are served by this run.
            tokio::select! {
                biased;
                _ = self.notify.notified() => {}
                _ = std::future::ready(()) => {}
            }
            let started = Instant::now();
            f().await;
            tokio::select! {
                _ = self.notify.notified() => {}
                _ = tokio::time::sleep(self.max_interval) => {}
            }
            let since = started.elapsed();
            if since < self.min_interval {
                tokio::time::sleep(self.min_interval - since).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(start_paused = true)]
    async fn coalesces_and_rate_limits() {
        let r = Arc::new(BoundedFrequencyRunner::new(Duration::from_secs(1), Duration::from_secs(30)));
        let n = Arc::new(AtomicUsize::new(0));
        let (r2, n2) = (r.clone(), n.clone());
        tokio::spawn(async move {
            r2.run(|| {
                let n = n2.clone();
                async move {
                    n.fetch_add(1, Ordering::SeqCst);
                }
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(n.load(Ordering::SeqCst), 1); // initial run
        for _ in 0..5 {
            r.trigger().notify_one();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(n.load(Ordering::SeqCst), 1); // rate limited
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(n.load(Ordering::SeqCst), 2); // five triggers → one run
        tokio::time::sleep(Duration::from_secs(31)).await;
        assert_eq!(n.load(Ordering::SeqCst), 3); // periodic resync
    }
}
