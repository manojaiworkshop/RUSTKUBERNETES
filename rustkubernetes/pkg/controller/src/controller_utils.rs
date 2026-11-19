//! Port of `pkg/controller/controller_utils.go` and `controller_ref_manager.go`:
//! work-queue plumbing, expectations, pod creation from templates, adoption,
//! and pod readiness/availability helpers shared by the workload controllers.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use k8s_api::core::v1::{Pod, PodTemplateSpec};
use k8s_api::Resource;
use k8s_apimachinery::labels::Selector;
use k8s_apimachinery::meta::{parse_time, ObjectMeta, OwnerReference};
use k8s_client_go::tools::cache::{CacheObject, Event};
use k8s_client_go::util::workqueue::WorkQueue;
use k8s_client_go::{Api, PatchType};
use serde_json::json;
use tokio::sync::broadcast;

pub type Queue = Arc<WorkQueue<String>>;

/// Result of a sync: `Ok(Some(d))` re-enqueues the key after `d`.
pub type SyncResult = anyhow::Result<Option<Duration>>;

/// An error that is retried with backoff but not reported (`certificates.IgnorableError`).
#[derive(Debug)]
pub struct Ignorable(pub String);

impl std::fmt::Display for Ignorable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Ignorable {}

/// Runs `workers` workers pulling keys from `queue` (`processNextWorkItem`).
pub async fn run_workers<F, Fut>(name: &'static str, queue: Queue, workers: usize, sync: F)
where
    F: Fn(String) -> Fut + Send + Sync + Clone + 'static,
    Fut: Future<Output = SyncResult> + Send + 'static,
{
    tracing::info!(controller = name, workers, "starting workers");
    let mut handles = vec![];
    for _ in 0..workers.max(1) {
        let (q, f) = (queue.clone(), sync.clone());
        handles.push(tokio::spawn(async move {
            while let Some(key) = q.get().await {
                match f(key.clone()).await {
                    Ok(None) => q.forget(&key),
                    Ok(Some(after)) => {
                        q.forget(&key);
                        q.add_after(key.clone(), after);
                    }
                    // Optimistic-concurrency conflicts are expected with a stale cache: retry quietly.
                    Err(e) if e.downcast_ref::<k8s_client_go::Error>().is_some_and(|ce| ce.is_conflict()) => {
                        tracing::debug!(controller = name, key = %key, "conflict, requeuing: {e}");
                        q.add_rate_limited(key.clone());
                    }
                    // Creating content in a namespace that is being deleted: nothing to do.
                    Err(e) if e.to_string().contains("because it is being terminated") => {
                        tracing::debug!(controller = name, key = %key, "namespace terminating: {e}");
                        q.forget(&key);
                    }
                    // `IgnorableError` (certificates): expected, retried with backoff, not logged as a failure.
                    Err(e) if e.downcast_ref::<Ignorable>().is_some() => {
                        tracing::debug!(controller = name, key = %key, "sync failed: {e}");
                        q.add_rate_limited(key.clone());
                    }
                    Err(e) => {
                        if q.num_requeues(&key) < 15 {
                            tracing::warn!(controller = name, key = %key, "sync failed, requeuing: {e:#}");
                            q.add_rate_limited(key.clone());
                        } else {
                            tracing::error!(controller = name, key = %key, "dropping key after repeated failures: {e:#}");
                            q.forget(&key);
                        }
                    }
                }
                q.done(&key);
            }
        }));
    }
    futures::future::join_all(handles).await;
}

/// Feeds informer events into a handler closure on a background task.
pub fn on_events<K, F>(mut rx: broadcast::Receiver<Event<K>>, handler: F)
where
    K: Send + Sync + 'static,
    F: Fn(Event<K>) + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => handler(ev),
                // Periodic resync covers missed events.
                Err(broadcast::error::RecvError::Lagged(n)) => tracing::warn!("event handler lagged by {n} events"),
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// Periodically enqueues every key returned by `keys` (informer resync).
pub fn resync_every(queue: Queue, period: Duration, keys: impl Fn() -> Vec<String> + Send + 'static) {
    tokio::spawn(async move {
        let mut t = tokio::time::interval(period);
        loop {
            t.tick().await;
            for k in keys() {
                queue.add(k);
            }
        }
    });
}

/// The controller owner reference of an object.
pub fn controller_ref(meta: &ObjectMeta) -> Option<&OwnerReference> {
    meta.controller_ref()
}

/// `metav1.NewControllerRef`
pub fn new_controller_ref<K: Resource>(owner: &K) -> OwnerReference {
    OwnerReference {
        api_version: K::api_version(),
        kind: K::KIND.into(),
        name: owner.metadata().name().into(),
        uid: owner.metadata().uid.clone().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    }
}

/// `namespace/name` of an object's controller if it is of kind `kind`.
pub fn owner_key(meta: &ObjectMeta, kind: &str) -> Option<String> {
    let r = meta.controller_ref()?;
    (r.kind == kind).then(|| format!("{}/{}", meta.namespace(), r.name))
}

// ---------------------------------------------------------------------------
// Expectations
// ---------------------------------------------------------------------------

const EXPECTATIONS_TTL: Duration = Duration::from_secs(5 * 60);

/// `ControllerExpectations`: after creating/deleting N pods a controller waits
/// to *observe* those N events before trusting its (stale) cache again.
#[derive(Default)]
pub struct Expectations {
    inner: Mutex<HashMap<String, (i64, i64, Instant)>>,
}

impl Expectations {
    pub fn expect(&self, key: &str, adds: i64, dels: i64) {
        self.inner.lock().unwrap().insert(key.into(), (adds, dels, Instant::now()));
    }

    pub fn creation_observed(&self, key: &str) {
        if let Some(e) = self.inner.lock().unwrap().get_mut(key) {
            e.0 -= 1;
        }
    }

    pub fn deletion_observed(&self, key: &str) {
        if let Some(e) = self.inner.lock().unwrap().get_mut(key) {
            e.1 -= 1;
        }
    }

    /// Lowers expectations for creations that failed.
    pub fn lower(&self, key: &str, adds: i64, dels: i64) {
        if let Some(e) = self.inner.lock().unwrap().get_mut(key) {
            e.0 -= adds;
            e.1 -= dels;
        }
    }

    pub fn satisfied(&self, key: &str) -> bool {
        match self.inner.lock().unwrap().get(key) {
            None => true,
            Some((a, d, t)) => (*a <= 0 && *d <= 0) || t.elapsed() > EXPECTATIONS_TTL,
        }
    }

    pub fn delete(&self, key: &str) {
        self.inner.lock().unwrap().remove(key);
    }
}

// ---------------------------------------------------------------------------
// Pods
// ---------------------------------------------------------------------------

pub fn is_pod_active(p: &Pod) -> bool {
    let phase = p.status.as_ref().and_then(|s| s.phase.as_deref()).unwrap_or("");
    phase != "Succeeded" && phase != "Failed" && p.metadata.deletion_timestamp.is_none()
}

pub fn is_pod_terminating(p: &Pod) -> bool {
    p.metadata.deletion_timestamp.is_some()
}

pub fn pod_phase(p: &Pod) -> &str {
    p.status.as_ref().and_then(|s| s.phase.as_deref()).unwrap_or("Pending")
}

fn ready_condition(p: &Pod) -> Option<&k8s_api::core::v1::PodCondition> {
    p.status.as_ref()?.conditions.as_ref()?.iter().find(|c| c.type_ == "Ready")
}

pub fn is_pod_ready(p: &Pod) -> bool {
    ready_condition(p).is_some_and(|c| c.status == "True")
}

/// Ready for at least `min_ready_seconds` (`IsPodAvailable`).
pub fn is_pod_available(p: &Pod, min_ready_seconds: i32) -> bool {
    let Some(c) = ready_condition(p).filter(|c| c.status == "True") else { return false };
    if min_ready_seconds <= 0 {
        return true;
    }
    c.last_transition_time
        .as_deref()
        .and_then(parse_time)
        .is_some_and(|t| t + chrono::Duration::seconds(min_ready_seconds as i64) <= Utc::now())
}

pub fn restart_count(p: &Pod) -> i32 {
    p.status.as_ref().and_then(|s| s.container_statuses.as_ref()).map(|cs| cs.iter().map(|c| c.restart_count).sum()).unwrap_or(0)
}

/// Sort pods so that the ones to delete first come first (`ActivePods` ordering):
/// unscheduled < scheduled, Pending < Unknown < Running, not ready < ready,
/// more restarts first, newer first.
pub fn rank_for_deletion(pods: &mut [Arc<Pod>]) {
    let phase_rank = |p: &Pod| match pod_phase(p) {
        "Pending" => 0,
        "Unknown" => 1,
        "Running" => 2,
        _ => 3,
    };
    pods.sort_by(|a, b| {
        let assigned = |p: &Pod| p.spec.as_ref().and_then(|s| s.node_name.as_ref()).is_some();
        assigned(a)
            .cmp(&assigned(b))
            .then(phase_rank(a).cmp(&phase_rank(b)))
            .then(is_pod_ready(a).cmp(&is_pod_ready(b)))
            .then(restart_count(b).cmp(&restart_count(a)))
            .then(b.metadata.creation_timestamp.cmp(&a.metadata.creation_timestamp))
    });
}

/// Builds a pod from a template (`GetPodFromTemplate`).
pub fn pod_from_template(template: &PodTemplateSpec, namespace: &str, generate_name: &str, owner: Option<OwnerReference>) -> Pod {
    let tmeta = template.metadata.clone().unwrap_or_default();
    let mut pod = Pod::new(ObjectMeta {
        generate_name: Some(generate_name.into()),
        namespace: Some(namespace.into()),
        labels: tmeta.labels,
        annotations: tmeta.annotations,
        finalizers: tmeta.finalizers,
        owner_references: owner.map(|o| vec![o]),
        ..Default::default()
    });
    pod.spec = template.spec.clone();
    pod
}

pub fn selector_of(ls: Option<&k8s_apimachinery::meta::LabelSelector>) -> Selector {
    match ls {
        Some(ls) => Selector::from_label_selector(ls).unwrap_or_else(|_| Selector::nothing()),
        None => Selector::nothing(),
    }
}

/// `PodControllerRefManager.ClaimPods`: returns the pods that belong to the owner,
/// adopting matching orphans and releasing owned pods that no longer match.
pub async fn claim_pods(
    pods_api_for_ns: &Api<Pod>,
    pods: Vec<Arc<Pod>>,
    owner: &OwnerReference,
    selector: &Selector,
    owner_deleting: bool,
) -> anyhow::Result<Vec<Arc<Pod>>> {
    let mut claimed = vec![];
    for p in pods {
        let matches = selector.matches(&p.metadata.labels());
        match p.metadata.controller_ref() {
            Some(r) if r.uid == owner.uid => {
                if matches {
                    claimed.push(p);
                } else if !owner_deleting {
                    release(pods_api_for_ns, &p.metadata, &owner.uid).await?;
                }
            }
            Some(_) => {}
            None => {
                if matches && !owner_deleting && p.metadata.deletion_timestamp.is_none() {
                    adopt(pods_api_for_ns, &p.metadata, owner).await?;
                    claimed.push(p);
                }
            }
        }
    }
    Ok(claimed)
}

/// Adds `owner` as controller of the object (merge patch with RV precondition).
pub async fn adopt<K: Resource>(api: &Api<K>, meta: &ObjectMeta, owner: &OwnerReference) -> anyhow::Result<()> {
    let mut refs = meta.owner_references.clone().unwrap_or_default();
    refs.push(owner.clone());
    let patch = json!({"metadata": {"ownerReferences": refs, "uid": meta.uid, "resourceVersion": meta.resource_version}});
    match api.patch(meta.name(), PatchType::Merge, &patch).await {
        Ok(_) => Ok(()),
        Err(e) if e.is_not_found() || e.is_conflict() => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Removes the owner reference with `owner_uid`.
pub async fn release<K: Resource>(api: &Api<K>, meta: &ObjectMeta, owner_uid: &str) -> anyhow::Result<()> {
    let refs: Vec<_> = meta.owner_references.clone().unwrap_or_default().into_iter().filter(|r| r.uid != owner_uid).collect();
    let patch = json!({"metadata": {"ownerReferences": refs, "resourceVersion": meta.resource_version}});
    match api.patch(meta.name(), PatchType::Merge, &patch).await {
        Ok(_) => Ok(()),
        Err(e) if e.is_not_found() || e.is_conflict() => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Objects of a store owned (controllerRef uid) by `owner_uid` in `namespace`.
pub fn owned_by<K: Resource>(items: Vec<Arc<K>>, owner_uid: &str) -> Vec<Arc<K>> {
    items.into_iter().filter(|o| o.metadata().controller_ref().is_some_and(|r| r.uid == owner_uid)).collect()
}

/// Semantic normalisation of a pod template for comparison and hashing: drops
/// `false`, `0`, `""` and `null` scalars. Go's protobuf encoder writes these for
/// every non-pointer field while JSON omits them, so the same template can arrive
/// in two shapes; `apiequality.Semantic.DeepEqual` treats them as equal too.
pub fn normalize_template(v: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .filter(|(_, x)| !matches!(x, Value::Null | Value::Bool(false)) && x.as_str() != Some("") && x.as_f64() != Some(0.0))
                .map(|(k, x)| (k.clone(), normalize_template(x)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(normalize_template).collect()),
        other => other.clone(),
    }
}

/// `ComputeHash`: FNV-32a of the template + collision count, safe-encoded
/// (used for `pod-template-hash` and revision hashes).
pub fn compute_hash(template: &serde_json::Value, collision_count: Option<i32>) -> String {
    let mut h: u32 = 0x811c9dc5;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            h ^= *b as u32;
            h = h.wrapping_mul(0x01000193);
        }
    };
    feed(normalize_template(template).to_string().as_bytes());
    if let Some(c) = collision_count {
        feed(&c.to_le_bytes());
    }
    safe_encode(&h.to_string())
}

/// `rand.SafeEncodeString`: maps each byte to a non-vowel alphanumeric.
pub fn safe_encode(s: &str) -> String {
    const ALPHANUMS: &[u8] = b"bcdfghjklmnpqrstvwxz2456789";
    s.bytes().map(|b| ALPHANUMS[b as usize % ALPHANUMS.len()] as char).collect()
}

/// Number of pods to create in this round of slow-start batching (1, 2, 4, ... capped).
pub fn slow_start_batches(total: usize) -> Vec<usize> {
    let mut out = vec![];
    let mut remaining = total;
    let mut batch = 1;
    while remaining > 0 {
        let n = batch.min(remaining);
        out.push(n);
        remaining -= n;
        batch *= 2;
    }
    out
}

/// `Toleration.ToleratesTaint`
pub fn tolerates(t: &k8s_api::core::v1::Toleration, taint: &k8s_api::core::v1::Taint) -> bool {
    if let Some(e) = t.effect.as_deref().filter(|e| !e.is_empty()) {
        if e != taint.effect {
            return false;
        }
    }
    let key = t.key.as_deref().unwrap_or("");
    match t.operator.as_deref().unwrap_or("Equal") {
        "Exists" => key.is_empty() || key == taint.key,
        _ => key == taint.key && t.value.as_deref().unwrap_or("") == taint.value.as_deref().unwrap_or(""),
    }
}

/// Taints of `effects` that no toleration matches.
pub fn untolerated<'a>(
    taints: &'a [k8s_api::core::v1::Taint],
    tolerations: &[k8s_api::core::v1::Toleration],
    effects: &[&str],
) -> Vec<&'a k8s_api::core::v1::Taint> {
    taints.iter().filter(|t| effects.contains(&t.effect.as_str()) && !tolerations.iter().any(|tol| tolerates(tol, t))).collect()
}

/// Labels as a map (empty if none).
pub fn labels_of(meta: &ObjectMeta) -> BTreeMap<String, String> {
    meta.labels()
}

/// Seconds since the RFC3339 timestamp (0 if unparsable).
pub fn seconds_since(ts: Option<&str>) -> i64 {
    ts.and_then(parse_time).map(|t| (Utc::now() - t).num_seconds()).unwrap_or(0)
}

/// Key of an informer object.
pub fn key<K: CacheObject>(o: &K) -> String {
    o.cache_key()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_and_hash() {
        assert_eq!(slow_start_batches(10), [1, 2, 4, 3]);
        assert_eq!(slow_start_batches(0), Vec::<usize>::new());
        let a = compute_hash(&json!({"a": 1}), None);
        assert_eq!(a, compute_hash(&json!({"a": 1}), None));
        assert_ne!(a, compute_hash(&json!({"a": 1}), Some(1)));
        assert!(a.chars().all(|c| "bcdfghjklmnpqrstvwxz2456789".contains(c)));
        // protobuf-shaped and JSON-shaped templates hash the same
        assert_eq!(
            compute_hash(&json!({"spec": {"hostIPC": false, "x": 1}, "metadata": {"generation": 0}}), None),
            compute_hash(&json!({"spec": {"x": 1}, "metadata": {}}), None)
        );
    }

    #[test]
    fn expectations() {
        let e = Expectations::default();
        assert!(e.satisfied("a"));
        e.expect("a", 2, 0);
        assert!(!e.satisfied("a"));
        e.creation_observed("a");
        e.creation_observed("a");
        assert!(e.satisfied("a"));
    }
}
