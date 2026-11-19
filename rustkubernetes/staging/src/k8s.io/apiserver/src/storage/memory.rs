//! In-memory implementation of [`Storage`] with optional snapshot persistence.
//!
//! It plays the role of etcd in phase 1: a single global revision counter,
//! an ordered key space, a bounded event history (like etcd's compaction
//! window) and fan-out of events to watchers (like the watch cache).
//!
//! With a data directory, the full state is snapshotted to `state.json`
//! (debounced, atomic rename), so the cluster survives restarts. Watch
//! history is not persisted: watchers older than a restart get `410 Gone`
//! and must re-list, exactly as after an etcd compaction.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use k8s_apimachinery::watch::EventType;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

use super::{Revision, Storage, StorageError, StoredEvent, WatchStream};

const DEFAULT_HISTORY: usize = 10_000;
const BROADCAST_CAPACITY: usize = 16_384;

struct Inner {
    objects: BTreeMap<String, Value>,
    history: VecDeque<StoredEvent>,
    dirty: bool,
}

pub struct MemoryStore {
    inner: Mutex<Inner>,
    revision: AtomicU64,
    tx: broadcast::Sender<StoredEvent>,
    history_limit: usize,
    snapshot_path: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    revision: Revision,
    objects: BTreeMap<String, Value>,
}

fn set_rv(obj: &mut Value, rev: Revision) {
    if let Some(meta) = obj.get_mut("metadata").and_then(Value::as_object_mut) {
        meta.insert("resourceVersion".into(), Value::String(rev.to_string()));
    } else if let Some(o) = obj.as_object_mut() {
        o.insert("metadata".into(), serde_json::json!({"resourceVersion": rev.to_string()}));
    }
}

fn get_rv(obj: &Value) -> Revision {
    obj.pointer("/metadata/resourceVersion").and_then(Value::as_str).and_then(|s| s.parse().ok()).unwrap_or(0)
}

impl MemoryStore {
    /// A purely in-memory store (state is lost on exit).
    pub fn new() -> Arc<Self> {
        Arc::new(Self::build(None, BTreeMap::new(), 0))
    }

    /// A store persisted under `dir/state.json`. Loads existing state and
    /// starts a background task that writes snapshots once per second when dirty.
    pub fn open(dir: &Path) -> Result<Arc<Self>, StorageError> {
        std::fs::create_dir_all(dir).map_err(|e| StorageError::Internal(format!("create {dir:?}: {e}")))?;
        let path = dir.join("state.json");
        let (objects, revision) = match std::fs::read(&path) {
            Ok(bytes) => {
                let snap: Snapshot =
                    serde_json::from_slice(&bytes).map_err(|e| StorageError::Internal(format!("corrupt snapshot {path:?}: {e}")))?;
                (snap.objects, snap.revision)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (BTreeMap::new(), 0),
            Err(e) => return Err(StorageError::Internal(format!("read {path:?}: {e}"))),
        };
        // Snapshots written before the upstream key layout used `/registry/<plural>.<group>/`.
        let migrated: BTreeMap<String, Value> = objects.into_iter().map(|(k, v)| (upstream_key(&k), v)).collect();
        let objects = migrated;
        tracing::info!(objects = objects.len(), revision, path = %path.display(), "loaded storage snapshot");
        let store = Arc::new(Self::build(Some(path), objects, revision));
        let weak = Arc::downgrade(&store);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                let Some(s) = weak.upgrade() else { break };
                if let Err(e) = s.flush_snapshot() {
                    tracing::error!("snapshot failed: {e}");
                }
            }
        });
        Ok(store)
    }

    fn build(snapshot_path: Option<PathBuf>, objects: BTreeMap<String, Value>, revision: Revision) -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            inner: Mutex::new(Inner { objects, history: VecDeque::new(), dirty: false }),
            revision: AtomicU64::new(revision),
            tx,
            history_limit: DEFAULT_HISTORY,
            snapshot_path,
        }
    }

    /// Writes the snapshot if anything changed since the last write.
    pub fn flush_snapshot(&self) -> Result<(), StorageError> {
        let Some(path) = &self.snapshot_path else { return Ok(()) };
        let bytes = {
            let mut inner = self.inner.lock().unwrap();
            if !inner.dirty {
                return Ok(());
            }
            inner.dirty = false;
            let snap = Snapshot { revision: self.revision.load(Ordering::SeqCst), objects: inner.objects.clone() };
            serde_json::to_vec(&snap).map_err(|e| StorageError::Internal(e.to_string()))?
        };
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes).map_err(|e| StorageError::Internal(format!("write {tmp:?}: {e}")))?;
        std::fs::rename(&tmp, path).map_err(|e| StorageError::Internal(format!("rename {tmp:?}: {e}")))?;
        Ok(())
    }

    /// Records an event; must be called with the lock held.
    fn record(&self, inner: &mut Inner, ev: StoredEvent) {
        inner.dirty = true;
        inner.history.push_back(ev.clone());
        while inner.history.len() > self.history_limit {
            inner.history.pop_front();
        }
        // No receivers is fine.
        let _ = self.tx.send(ev);
    }

    fn next_rev(&self) -> Revision {
        self.revision.fetch_add(1, Ordering::SeqCst) + 1
    }
}

#[async_trait]
impl Storage for MemoryStore {
    fn flush(&self) -> Result<(), StorageError> {
        self.flush_snapshot()
    }

    async fn create(&self, key: &str, mut obj: Value) -> Result<Value, StorageError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.objects.contains_key(key) {
            return Err(StorageError::AlreadyExists(key.into()));
        }
        let rev = self.next_rev();
        set_rv(&mut obj, rev);
        inner.objects.insert(key.to_string(), obj.clone());
        self.record(&mut inner, StoredEvent { type_: EventType::Added, key: key.into(), object: obj.clone(), prev: None, revision: rev });
        Ok(obj)
    }

    async fn get(&self, key: &str) -> Result<Value, StorageError> {
        let inner = self.inner.lock().unwrap();
        inner.objects.get(key).cloned().ok_or_else(|| StorageError::NotFound(key.into()))
    }

    async fn list(&self, prefix: &str) -> Result<(Vec<Value>, Revision), StorageError> {
        let inner = self.inner.lock().unwrap();
        let items = inner.objects.range(prefix.to_string()..).take_while(|(k, _)| k.starts_with(prefix)).map(|(_, v)| v.clone()).collect();
        Ok((items, self.revision.load(Ordering::SeqCst)))
    }

    async fn update(&self, key: &str, expected: Option<Revision>, mut obj: Value) -> Result<Value, StorageError> {
        let mut inner = self.inner.lock().unwrap();
        let prev = inner.objects.get(key).cloned().ok_or_else(|| StorageError::NotFound(key.into()))?;
        if let Some(exp) = expected {
            if get_rv(&prev) != exp {
                return Err(StorageError::Conflict(key.into()));
            }
        }
        let rev = self.next_rev();
        set_rv(&mut obj, rev);
        inner.objects.insert(key.to_string(), obj.clone());
        self.record(
            &mut inner,
            StoredEvent { type_: EventType::Modified, key: key.into(), object: obj.clone(), prev: Some(prev), revision: rev },
        );
        Ok(obj)
    }

    async fn delete(&self, key: &str, expected: Option<Revision>) -> Result<Value, StorageError> {
        let mut inner = self.inner.lock().unwrap();
        let prev = inner.objects.get(key).cloned().ok_or_else(|| StorageError::NotFound(key.into()))?;
        if let Some(exp) = expected {
            if get_rv(&prev) != exp {
                return Err(StorageError::Conflict(key.into()));
            }
        }
        inner.objects.remove(key);
        let rev = self.next_rev();
        let mut obj = prev.clone();
        set_rv(&mut obj, rev);
        self.record(
            &mut inner,
            StoredEvent { type_: EventType::Deleted, key: key.into(), object: obj.clone(), prev: Some(prev), revision: rev },
        );
        Ok(obj)
    }

    async fn watch(&self, prefix: &str, from: Revision) -> Result<WatchStream, StorageError> {
        let (replay, mut rx) = {
            let inner = self.inner.lock().unwrap();
            let current = self.revision.load(Ordering::SeqCst);
            if from > current {
                // A revision this store never issued (e.g. a client that watched a
                // previous server whose last writes were not persisted): force a relist
                // instead of silently skipping events up to `from`.
                return Err(StorageError::Gone { requested: from, oldest: current });
            }
            if from < current {
                // Need every event in (from, current]; check the history covers it.
                let oldest = inner.history.front().map(|e| e.revision).unwrap_or(current + 1);
                if oldest > from + 1 {
                    return Err(StorageError::Gone { requested: from, oldest });
                }
            }
            let replay: Vec<StoredEvent> =
                inner.history.iter().filter(|e| e.revision > from && e.key.starts_with(prefix)).cloned().collect();
            // Subscribe while holding the lock so no event can slip between replay and live.
            (replay, self.tx.subscribe())
        };
        let prefix = prefix.to_string();
        let stream = async_stream::stream! {
            let mut last = from;
            for ev in replay {
                last = ev.revision;
                yield Ok(ev);
            }
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if ev.revision <= last || !ev.key.starts_with(&prefix) {
                            continue;
                        }
                        last = ev.revision;
                        yield Ok(ev);
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        yield Err(StorageError::Gone { requested: last, oldest: last + 1 });
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        };
        Ok(Box::pin(stream))
    }

    fn current_revision(&self) -> Revision {
        self.revision.load(Ordering::SeqCst)
    }
}

/// Old key layout → upstream etcd layout (`deployments.apps` → `deployments`,
/// `nodes` → `minions`, `services` → `services/specs`, `endpoints` → `services/endpoints`).
pub fn upstream_key(k: &str) -> String {
    let Some(rest) = k.strip_prefix("/registry/") else { return k.to_string() };
    let (seg, tail) = rest.split_once('/').unwrap_or((rest, ""));
    let seg = seg.split('.').next().unwrap_or(seg);
    let new = match seg {
        "nodes" => "minions",
        "endpoints" => "services/endpoints",
        "services" if !(tail.starts_with("specs/") || tail.starts_with("endpoints/")) => "services/specs",
        s => s,
    };
    format!("/registry/{new}/{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use serde_json::json;

    #[test]
    fn migrates_old_key_layout() {
        assert_eq!(upstream_key("/registry/deployments.apps/ns/web"), "/registry/deployments/ns/web");
        assert_eq!(upstream_key("/registry/nodes/n1"), "/registry/minions/n1");
        assert_eq!(upstream_key("/registry/services/default/kubernetes"), "/registry/services/specs/default/kubernetes");
        assert_eq!(upstream_key("/registry/services/specs/default/kubernetes"), "/registry/services/specs/default/kubernetes");
        assert_eq!(upstream_key("/registry/endpoints/default/kubernetes"), "/registry/services/endpoints/default/kubernetes");
        assert_eq!(upstream_key("/registry/pods/ns/p"), "/registry/pods/ns/p");
    }

    #[tokio::test]
    async fn conformance() {
        super::super::conformance::run(MemoryStore::new()).await;
    }

    #[tokio::test]
    async fn crud_and_conflicts() {
        let s = MemoryStore::new();
        let o = s.create("/registry/pods/ns/a", json!({"metadata": {"name": "a"}})).await.unwrap();
        assert_eq!(o["metadata"]["resourceVersion"], "1");
        assert!(matches!(s.create("/registry/pods/ns/a", json!({})).await, Err(StorageError::AlreadyExists(_))));
        assert!(matches!(s.update("/registry/pods/ns/a", Some(99), json!({"metadata": {}})).await, Err(StorageError::Conflict(_))));
        let o = s.update("/registry/pods/ns/a", Some(1), json!({"metadata": {"name": "a"}})).await.unwrap();
        assert_eq!(o["metadata"]["resourceVersion"], "2");
        s.create("/registry/pods/other/b", json!({"metadata": {}})).await.unwrap();
        assert_eq!(s.list("/registry/pods/ns/").await.unwrap().0.len(), 1);
        assert_eq!(s.list("/registry/pods/").await.unwrap().0.len(), 2);
        let d = s.delete("/registry/pods/ns/a", None).await.unwrap();
        assert_eq!(d["metadata"]["resourceVersion"], "4");
        assert!(matches!(s.get("/registry/pods/ns/a").await, Err(StorageError::NotFound(_))));
    }

    #[tokio::test]
    async fn watch_replays_then_streams() {
        let s = MemoryStore::new();
        s.create("/registry/pods/ns/a", json!({"metadata": {}})).await.unwrap(); // rev 1
        s.create("/registry/pods/ns/b", json!({"metadata": {}})).await.unwrap(); // rev 2
        let mut w = s.watch("/registry/pods/ns/", 1).await.unwrap();
        let e = w.next().await.unwrap().unwrap();
        assert_eq!((e.type_, e.revision), (EventType::Added, 2));
        s.delete("/registry/pods/ns/a", None).await.unwrap(); // rev 3
        s.create("/registry/cm/ns/x", json!({"metadata": {}})).await.unwrap(); // other prefix
        let e = w.next().await.unwrap().unwrap();
        assert_eq!((e.type_, e.revision), (EventType::Deleted, 3));
    }

    #[tokio::test]
    async fn persistence_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = MemoryStore::open(dir.path()).unwrap();
            s.create("/registry/namespaces/x", json!({"metadata": {"name": "x"}})).await.unwrap();
            s.flush_snapshot().unwrap();
        }
        let s = MemoryStore::open(dir.path()).unwrap();
        assert_eq!(s.current_revision(), 1);
        assert_eq!(s.get("/registry/namespaces/x").await.unwrap()["metadata"]["name"], "x");
        // History is not persisted: watching from before the restart is Gone.
        s.create("/registry/namespaces/y", json!({"metadata": {}})).await.unwrap();
        assert!(matches!(s.watch("/registry/", 0).await, Err(StorageError::Gone { .. })));
    }

    #[tokio::test]
    async fn watch_from_future_revision_is_gone() {
        // A client that saw revision 50 on a previous server (whose last writes were
        // not persisted) must relist instead of silently missing events 1..=50.
        let s = MemoryStore::new();
        s.create("/registry/pods/ns/a", json!({"metadata": {}})).await.unwrap();
        assert!(matches!(s.watch("/registry/pods/", 50).await, Err(StorageError::Gone { .. })));
        assert!(s.watch("/registry/pods/", 1).await.is_ok());
    }
}
