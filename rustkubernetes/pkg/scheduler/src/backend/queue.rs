//! Port of `pkg/scheduler/backend/queue` (`PriorityQueue`).
//!
//! Three places a pending pod can be:
//! * **activeQ** — ready to be scheduled; popped by priority, then age (`PrioritySort`);
//! * **backoffQ** — failed recently; waits `1s · 2^(attempts-1)` (max 10s);
//! * **unschedulable** — no node fits; waits for a cluster event (`move_all`)
//!   or at most [`UNSCHEDULABLE_FLUSH`].
//!
//! If a cluster event arrives while a pod is being scheduled, a failure moves it
//! to backoff instead of unschedulable (it may fit now).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use k8s_api::core::v1::Pod;
use tokio::sync::Notify;

pub const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
pub const MAX_BACKOFF: Duration = Duration::from_secs(10);
pub const UNSCHEDULABLE_FLUSH: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Where {
    Active,
    Backoff(Instant),
    Unschedulable(Instant),
    /// Waiting on a PreEnqueue gate (scheduling gates).
    Gated,
    InFlight,
}

/// A pod in the queue (`QueuedPodInfo`).
#[derive(Clone, Debug)]
pub struct QueuedPod {
    pub pod: Arc<Pod>,
    pub attempts: u32,
    pub added: Instant,
    /// `move_seq` when this attempt started.
    cycle: u64,
}

impl QueuedPod {
    pub fn key(&self) -> String {
        pod_key(&self.pod)
    }
}

pub fn pod_key(p: &Pod) -> String {
    format!("{}/{}", p.metadata.namespace(), p.metadata.name())
}

fn priority(p: &Pod) -> i32 {
    p.spec.as_ref().and_then(|s| s.priority).unwrap_or(0)
}

struct State {
    pods: HashMap<String, (QueuedPod, Where)>,
    move_seq: u64,
    closed: bool,
}

pub struct SchedulingQueue {
    state: Mutex<State>,
    notify: Notify,
    gate: Box<dyn Fn(&Pod) -> bool + Send + Sync>,
}

fn backoff(attempts: u32) -> Duration {
    let exp = attempts.saturating_sub(1).min(10);
    (INITIAL_BACKOFF * 2u32.pow(exp)).min(MAX_BACKOFF)
}

impl SchedulingQueue {
    /// `gate(pod)` returns true if the pod may enter the active queue (PreEnqueue plugins).
    pub fn new(gate: impl Fn(&Pod) -> bool + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State { pods: HashMap::new(), move_seq: 0, closed: false }),
            notify: Notify::new(),
            gate: Box::new(gate),
        })
    }

    /// Adds a new pending pod (or refreshes it).
    pub fn add(&self, pod: Arc<Pod>) {
        let key = pod_key(&pod);
        let mut st = self.state.lock().unwrap();
        let w = if (self.gate)(&pod) { Where::Active } else { Where::Gated };
        match st.pods.get_mut(&key) {
            Some((qp, loc)) => {
                // Only a spec/label change can make a pod schedulable; status-only
                // updates (e.g. our own PodScheduled condition) must not requeue it.
                let relevant = qp.pod.spec != pod.spec || qp.pod.metadata.labels != pod.metadata.labels;
                qp.pod = pod;
                match *loc {
                    Where::Gated | Where::Unschedulable(_) if relevant => *loc = w,
                    _ => {}
                }
            }
            None => {
                st.pods.insert(key, (QueuedPod { pod, attempts: 0, added: Instant::now(), cycle: 0 }, w));
            }
        }
        drop(st);
        self.notify.notify_one();
    }

    pub fn delete(&self, key: &str) {
        self.state.lock().unwrap().pods.remove(key);
    }

    /// Bind succeeded (or the pod is gone): forget it.
    pub fn done(&self, key: &str) {
        let mut st = self.state.lock().unwrap();
        if matches!(st.pods.get(key), Some((_, Where::InFlight))) {
            st.pods.remove(key);
        }
    }

    /// Pops the highest-priority, oldest active pod, waiting if none is ready.
    pub async fn pop(&self) -> Option<QueuedPod> {
        loop {
            {
                let mut st = self.state.lock().unwrap();
                if st.closed {
                    return None;
                }
                let now = Instant::now();
                let seq = st.move_seq;
                for (_, (_, loc)) in st.pods.iter_mut() {
                    match *loc {
                        Where::Backoff(until) if until <= now => *loc = Where::Active,
                        Where::Unschedulable(since) if now.duration_since(since) >= UNSCHEDULABLE_FLUSH => *loc = Where::Active,
                        _ => {}
                    }
                }
                let best = st
                    .pods
                    .iter()
                    .filter(|(_, (_, l))| *l == Where::Active)
                    .max_by(|a, b| priority(&a.1 .0.pod).cmp(&priority(&b.1 .0.pod)).then(b.1 .0.added.cmp(&a.1 .0.added)))
                    .map(|(k, _)| k.clone());
                if let Some(k) = best {
                    let (qp, loc) = st.pods.get_mut(&k).unwrap();
                    *loc = Where::InFlight;
                    qp.attempts += 1;
                    qp.cycle = seq;
                    return Some(qp.clone());
                }
            }
            let _ = tokio::time::timeout(Duration::from_millis(500), self.notify.notified()).await;
        }
    }

    /// The pod did not fit anywhere.
    pub fn add_unschedulable(&self, qp: QueuedPod) {
        let mut st = self.state.lock().unwrap();
        let key = qp.key();
        let Some((cur, loc)) = st.pods.get_mut(&key) else { return };
        if *loc != Where::InFlight {
            return; // updated/deleted meanwhile
        }
        cur.attempts = qp.attempts;
        let moved_meanwhile = st.move_seq > qp.cycle;
        let (_, loc) = st.pods.get_mut(&key).unwrap();
        *loc = if moved_meanwhile { Where::Backoff(Instant::now() + backoff(qp.attempts)) } else { Where::Unschedulable(Instant::now()) };
    }

    /// The attempt failed with an error (e.g. binding): retry after backoff.
    pub fn add_backoff(&self, qp: QueuedPod) {
        let mut st = self.state.lock().unwrap();
        let key = qp.key();
        let entry = st.pods.entry(key).or_insert((qp.clone(), Where::InFlight));
        entry.1 = Where::Backoff(Instant::now() + backoff(qp.attempts));
    }

    /// A cluster event happened (node added, pod deleted, ...): retry unschedulable pods.
    pub fn move_all(&self, reason: &str) {
        let mut st = self.state.lock().unwrap();
        st.move_seq += 1;
        let mut moved = 0;
        for (_, (qp, loc)) in st.pods.iter_mut() {
            if let Where::Unschedulable(_) = *loc {
                *loc = Where::Backoff(Instant::now() + backoff(qp.attempts).min(Duration::from_millis(200)));
                moved += 1;
            }
        }
        drop(st);
        if moved > 0 {
            tracing::debug!(reason, moved, "moved unschedulable pods");
            self.notify.notify_one();
        }
    }

    pub fn len(&self) -> usize {
        self.state.lock().unwrap().pods.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Counts per sub-queue: (active, backoff, unschedulable, gated).
    pub fn stats(&self) -> (usize, usize, usize, usize) {
        let st = self.state.lock().unwrap();
        let mut c = (0, 0, 0, 0);
        for (_, l) in st.pods.values() {
            match l {
                Where::Active => c.0 += 1,
                Where::Backoff(_) => c.1 += 1,
                Where::Unschedulable(_) => c.2 += 1,
                Where::Gated => c.3 += 1,
                Where::InFlight => {}
            }
        }
        c
    }

    pub fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pod(name: &str, prio: i32) -> Arc<Pod> {
        Arc::new(
            serde_json::from_value(json!({"metadata": {"name": name, "namespace": "d"}, "spec": {"priority": prio, "containers": []}}))
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn priority_order_and_unschedulable() {
        let q = SchedulingQueue::new(|_| true);
        q.add(pod("low", 0));
        q.add(pod("high", 100));
        assert_eq!(q.pop().await.unwrap().pod.metadata.name(), "high");
        let low = q.pop().await.unwrap();
        assert_eq!(low.pod.metadata.name(), "low");
        q.add_unschedulable(low);
        assert_eq!(q.stats().2, 1);
        q.move_all("NodeAdd");
        let again = tokio::time::timeout(Duration::from_secs(2), q.pop()).await.unwrap().unwrap();
        assert_eq!(again.attempts, 2);
    }

    #[tokio::test]
    async fn gated_pods_wait() {
        let q = SchedulingQueue::new(|p| !p.metadata.labels().contains_key("gate"));
        let mut p: Pod = (*pod("g", 0)).clone();
        p.metadata.labels = Some([("gate".to_string(), "x".to_string())].into_iter().collect());
        q.add(Arc::new(p.clone()));
        assert_eq!(q.stats().3, 1);
        p.metadata.labels = None;
        q.add(Arc::new(p));
        assert_eq!(q.pop().await.unwrap().pod.metadata.name(), "g");
    }
}
