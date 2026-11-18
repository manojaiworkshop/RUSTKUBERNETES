//! Port of `client-go/util/workqueue`: a de-duplicating, rate-limited work queue.
//!
//! Guarantees (same as Go):
//! * an item is in the queue at most once (adds while queued are merged);
//! * an item is processed by at most one worker at a time — adds while it is
//!   being processed are re-queued when the worker calls [`WorkQueue::done`];
//! * [`WorkQueue::add_rate_limited`] applies per-item exponential backoff
//!   (5ms · 2^failures, max 1000s) until [`WorkQueue::forget`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

struct State<T> {
    queue: VecDeque<T>,
    dirty: HashSet<T>,
    processing: HashSet<T>,
    failures: HashMap<T, u32>,
    shutting_down: bool,
}

pub struct WorkQueue<T> {
    state: Mutex<State<T>>,
    notify: Notify,
    base_delay: Duration,
    max_delay: Duration,
}

impl<T: Hash + Eq + Clone + Send + 'static> WorkQueue<T> {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                queue: VecDeque::new(),
                dirty: HashSet::new(),
                processing: HashSet::new(),
                failures: HashMap::new(),
                shutting_down: false,
            }),
            notify: Notify::new(),
            base_delay: Duration::from_millis(5),
            max_delay: Duration::from_secs(1000),
        })
    }

    pub fn add(&self, item: T) {
        let mut s = self.state.lock().unwrap();
        if s.shutting_down || !s.dirty.insert(item.clone()) {
            return;
        }
        if s.processing.contains(&item) {
            return; // re-queued by done()
        }
        s.queue.push_back(item);
        drop(s);
        self.notify.notify_one();
    }

    /// Adds the item after `delay`.
    pub fn add_after(self: &Arc<Self>, item: T, delay: Duration) {
        if delay.is_zero() {
            return self.add(item);
        }
        let q = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            q.add(item);
        });
    }

    /// Adds with per-item exponential backoff.
    pub fn add_rate_limited(self: &Arc<Self>, item: T) {
        let n = {
            let mut s = self.state.lock().unwrap();
            let f = s.failures.entry(item.clone()).or_insert(0);
            *f += 1;
            *f
        };
        let delay = self.base_delay.saturating_mul(1u32 << (n - 1).min(31)).min(self.max_delay);
        self.add_after(item, delay);
    }

    /// Clears the failure count of an item (call after a successful sync).
    pub fn forget(&self, item: &T) {
        self.state.lock().unwrap().failures.remove(item);
    }

    pub fn num_requeues(&self, item: &T) -> u32 {
        self.state.lock().unwrap().failures.get(item).copied().unwrap_or(0)
    }

    /// Waits for the next item. Returns `None` after shutdown.
    pub async fn get(&self) -> Option<T> {
        loop {
            {
                let mut s = self.state.lock().unwrap();
                if let Some(item) = s.queue.pop_front() {
                    s.dirty.remove(&item);
                    s.processing.insert(item.clone());
                    return Some(item);
                }
                if s.shutting_down {
                    return None;
                }
            }
            self.notify.notified().await;
        }
    }

    /// Marks an item as processed; re-queues it if it was added meanwhile.
    pub fn done(&self, item: &T) {
        let mut s = self.state.lock().unwrap();
        s.processing.remove(item);
        if s.dirty.contains(item) {
            s.queue.push_back(item.clone());
            drop(s);
            self.notify.notify_one();
        }
    }

    pub fn len(&self) -> usize {
        self.state.lock().unwrap().queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn shutdown(&self) {
        self.state.lock().unwrap().shutting_down = true;
        self.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dedup_and_requeue_while_processing() {
        let q = WorkQueue::new();
        q.add("a");
        q.add("a");
        q.add("b");
        assert_eq!(q.len(), 2);
        let a = q.get().await.unwrap();
        assert_eq!(a, "a");
        q.add("a"); // while processing: not queued yet
        assert_eq!(q.len(), 1);
        q.done(&a); // now re-queued
        assert_eq!(q.len(), 2);
        assert_eq!(q.get().await.unwrap(), "b");
        assert_eq!(q.get().await.unwrap(), "a");
    }

    #[tokio::test]
    async fn rate_limited_and_shutdown() {
        let q = WorkQueue::new();
        q.add_rate_limited("x");
        assert_eq!(q.num_requeues(&"x"), 1);
        let got = tokio::time::timeout(Duration::from_secs(1), q.get()).await.unwrap();
        assert_eq!(got, Some("x"));
        q.forget(&"x");
        assert_eq!(q.num_requeues(&"x"), 0);
        let q2 = q.clone();
        let h = tokio::spawn(async move { q2.get().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        q.shutdown();
        assert_eq!(h.await.unwrap(), None);
    }
}
