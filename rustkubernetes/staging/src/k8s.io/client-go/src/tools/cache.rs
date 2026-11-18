//! Port of `client-go/tools/cache`: Reflector, Store (indexer) and Informer.
//!
//! An [`Informer`] keeps a local, eventually consistent copy of a resource:
//!
//! ```text
//!  LIST ──▶ replace Store ──▶ emit Added/Updated/Deleted (diff)
//!   │
//!  WATCH(resourceVersion) ──▶ apply event to Store ──▶ emit event
//!   │  410 Gone / error ──▶ back off and LIST again
//! ```
//!
//! Informers work for typed objects (`Informer<Pod>`, via [`Informer::new`])
//! and for any resource as untyped JSON (`Informer<DynamicObject>`, via
//! [`Informer::dynamic`]) — the latter is what the garbage collector uses.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use k8s_api::Resource;
use k8s_apimachinery::unstructured::ObjectExt;
use k8s_apimachinery::watch::EventType;
use serde_json::Value;
use tokio::sync::{broadcast, watch};

use crate::api::{Api, DynamicApi, ListParams};
use crate::rest::Error;

/// Anything an informer can cache.
pub trait CacheObject: Clone + Send + Sync + 'static {
    fn namespace(&self) -> &str;
    fn name(&self) -> &str;
    fn uid(&self) -> &str;
    fn resource_version(&self) -> Option<&str>;

    /// `namespace/name` for namespaced objects, `name` otherwise (`MetaNamespaceKeyFunc`).
    fn cache_key(&self) -> String {
        if self.namespace().is_empty() {
            self.name().to_string()
        } else {
            format!("{}/{}", self.namespace(), self.name())
        }
    }
}

impl<K: Resource> CacheObject for K {
    fn namespace(&self) -> &str {
        self.metadata().namespace()
    }
    fn name(&self) -> &str {
        self.metadata().name()
    }
    fn uid(&self) -> &str {
        self.metadata().uid.as_deref().unwrap_or("")
    }
    fn resource_version(&self) -> Option<&str> {
        self.metadata().resource_version.as_deref()
    }
}

/// An untyped object (`unstructured.Unstructured`).
#[derive(Clone, Debug, PartialEq)]
pub struct DynamicObject(pub Value);

impl CacheObject for DynamicObject {
    fn namespace(&self) -> &str {
        ObjectExt::namespace(&self.0)
    }
    fn name(&self) -> &str {
        ObjectExt::name(&self.0)
    }
    fn uid(&self) -> &str {
        ObjectExt::uid(&self.0)
    }
    fn resource_version(&self) -> Option<&str> {
        Some(ObjectExt::resource_version(&self.0)).filter(|s| !s.is_empty())
    }
}

/// `namespace/name` key of a typed object.
pub fn key_of<K: CacheObject>(obj: &K) -> String {
    obj.cache_key()
}

/// Splits a key back into `(namespace, name)`.
pub fn split_key(key: &str) -> (&str, &str) {
    key.split_once('/').unwrap_or(("", key))
}

/// Source of list + watch for a reflector (`cache.ListerWatcher`).
#[async_trait]
pub trait ListerWatcher<K>: Send + Sync {
    /// Returns the items and the list's resourceVersion.
    async fn list(&self) -> Result<(Vec<K>, String), Error>;
    async fn watch(&self, resource_version: &str, timeout_seconds: u32)
        -> Result<BoxStream<'static, Result<(EventType, K), Error>>, Error>;
}

struct TypedLw<K> {
    api: Api<K>,
    params: ListParams,
}

#[async_trait]
impl<K: Resource> ListerWatcher<K> for TypedLw<K> {
    async fn list(&self) -> Result<(Vec<K>, String), Error> {
        let l = self.api.list(&self.params).await?;
        Ok((l.items, l.metadata.resource_version.unwrap_or_default()))
    }
    async fn watch(&self, rv: &str, timeout: u32) -> Result<BoxStream<'static, Result<(EventType, K), Error>>, Error> {
        let mut p = self.params.clone();
        p.resource_version = Some(rv.to_string());
        p.timeout_seconds = Some(timeout);
        Ok(self.api.watch(&p).await?.map(|ev| ev.map(|e| (e.type_, e.object))).boxed())
    }
}

struct DynamicLw {
    api: DynamicApi,
    params: ListParams,
}

#[async_trait]
impl ListerWatcher<DynamicObject> for DynamicLw {
    async fn list(&self) -> Result<(Vec<DynamicObject>, String), Error> {
        let mut l = self.api.list(&self.params).await?;
        let rv = l["metadata"]["resourceVersion"].as_str().unwrap_or("").to_string();
        let kind = self.api.resource().kind.clone();
        let av = self.api.resource().api_version();
        let items = match l["items"].take() {
            Value::Array(a) => a
                .into_iter()
                .map(|mut o| {
                    // Lists may omit item kind/apiVersion; fill them in.
                    o["kind"] = Value::String(kind.clone());
                    o["apiVersion"] = Value::String(av.clone());
                    DynamicObject(o)
                })
                .collect(),
            _ => vec![],
        };
        Ok((items, rv))
    }
    async fn watch(&self, rv: &str, timeout: u32) -> Result<BoxStream<'static, Result<(EventType, DynamicObject), Error>>, Error> {
        let mut p = self.params.clone();
        p.resource_version = Some(rv.to_string());
        p.timeout_seconds = Some(timeout);
        Ok(self.api.watch(&p).await?.map(|ev| ev.map(|e| (e.type_, DynamicObject(e.object)))).boxed())
    }
}

/// Thread-safe local cache (`cache.Indexer`).
pub struct Store<K> {
    items: Arc<RwLock<BTreeMap<String, Arc<K>>>>,
}

impl<K> Clone for Store<K> {
    fn clone(&self) -> Self {
        Self { items: self.items.clone() }
    }
}

impl<K: CacheObject> Default for Store<K> {
    fn default() -> Self {
        Self { items: Arc::new(RwLock::new(BTreeMap::new())) }
    }
}

impl<K: CacheObject> Store<K> {
    pub fn get(&self, key: &str) -> Option<Arc<K>> {
        self.items.read().unwrap().get(key).cloned()
    }

    pub fn get_ns(&self, namespace: &str, name: &str) -> Option<Arc<K>> {
        if namespace.is_empty() {
            self.get(name)
        } else {
            self.get(&format!("{namespace}/{name}"))
        }
    }

    pub fn list(&self) -> Vec<Arc<K>> {
        self.items.read().unwrap().values().cloned().collect()
    }

    pub fn keys(&self) -> Vec<String> {
        self.items.read().unwrap().keys().cloned().collect()
    }

    /// Objects in one namespace (`ByIndex(NamespaceIndex, ns)`).
    pub fn list_namespace(&self, namespace: &str) -> Vec<Arc<K>> {
        let prefix = format!("{namespace}/");
        self.items.read().unwrap().range(prefix.clone()..).take_while(|(k, _)| k.starts_with(&prefix)).map(|(_, v)| v.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.items.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn upsert(&self, obj: K) -> (Arc<K>, Option<Arc<K>>) {
        let new = Arc::new(obj);
        let old = self.items.write().unwrap().insert(new.cache_key(), new.clone());
        (new, old)
    }

    fn remove(&self, key: &str) -> Option<Arc<K>> {
        self.items.write().unwrap().remove(key)
    }
}

/// A change observed by an informer (`ResourceEventHandler` callbacks).
#[derive(Debug)]
pub enum Event<K> {
    Added(Arc<K>),
    Updated { old: Arc<K>, new: Arc<K> },
    Deleted(Arc<K>),
}

impl<K> Clone for Event<K> {
    fn clone(&self) -> Self {
        match self {
            Event::Added(o) => Event::Added(o.clone()),
            Event::Updated { old, new } => Event::Updated { old: old.clone(), new: new.clone() },
            Event::Deleted(o) => Event::Deleted(o.clone()),
        }
    }
}

impl<K> Event<K> {
    /// The current object of the event.
    pub fn object(&self) -> &Arc<K> {
        match self {
            Event::Added(o) | Event::Deleted(o) => o,
            Event::Updated { new, .. } => new,
        }
    }
}

/// `SharedIndexInformer`: a reflector feeding a store plus event fan-out.
pub struct Informer<K: CacheObject> {
    lw: Box<dyn ListerWatcher<K>>,
    name: String,
    store: Store<K>,
    events: broadcast::Sender<Event<K>>,
    synced_tx: watch::Sender<bool>,
    synced_rx: watch::Receiver<bool>,
    started: std::sync::atomic::AtomicBool,
    task: std::sync::Mutex<Option<tokio::task::AbortHandle>>,
}

impl<K: Resource> Informer<K> {
    pub fn new(api: Api<K>, params: ListParams) -> Arc<Self> {
        Self::from_lister_watcher(K::KIND, Box::new(TypedLw { api, params }))
    }
}

impl Informer<DynamicObject> {
    /// An informer over any resource, as untyped JSON.
    pub fn dynamic(api: DynamicApi, params: ListParams) -> Arc<Self> {
        let name = api.resource().kind.clone();
        Self::from_lister_watcher(&name, Box::new(DynamicLw { api, params }))
    }
}

impl<K: CacheObject> Informer<K> {
    pub fn from_lister_watcher(name: &str, lw: Box<dyn ListerWatcher<K>>) -> Arc<Self> {
        let (events, _) = broadcast::channel(16384);
        let (synced_tx, synced_rx) = watch::channel(false);
        Arc::new(Self {
            lw,
            name: name.to_string(),
            store: Store::default(),
            events,
            synced_tx,
            synced_rx,
            started: std::sync::atomic::AtomicBool::new(false),
            task: std::sync::Mutex::new(None),
        })
    }

    pub fn store(&self) -> Store<K> {
        self.store.clone()
    }

    /// Subscribe before [`Informer::start`] to see the initial `Added` events.
    /// A lagging receiver gets `RecvError::Lagged` and should resync from the store.
    pub fn subscribe(&self) -> broadcast::Receiver<Event<K>> {
        self.events.subscribe()
    }

    pub fn has_synced(&self) -> bool {
        *self.synced_rx.borrow()
    }

    /// Waits until the first LIST has been delivered (`WaitForCacheSync`).
    pub async fn wait_synced(&self) {
        let mut rx = self.synced_rx.clone();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// Spawns the reflector loop on the current Tokio runtime (idempotent).
    pub fn start(self: &Arc<Self>) {
        if self.started.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        let h = tokio::spawn(async move { me.run().await });
        *self.task.lock().unwrap() = Some(h.abort_handle());
    }

    /// Stops the reflector loop (the store keeps its last contents).
    pub fn stop(&self) {
        if let Some(h) = self.task.lock().unwrap().take() {
            h.abort();
        }
    }

    fn emit(&self, ev: Event<K>) {
        let _ = self.events.send(ev);
    }

    /// LIST and replace the store, emitting the differences. Returns the list resourceVersion.
    async fn relist(&self) -> Result<String, Error> {
        let (items, rv) = self.lw.list().await?;
        let mut seen = std::collections::HashSet::new();
        for obj in items {
            seen.insert(obj.cache_key());
            let new_rv = obj.resource_version().map(String::from);
            match self.store.upsert(obj) {
                (new, None) => self.emit(Event::Added(new)),
                (new, Some(old)) => {
                    if old.resource_version().map(String::from) != new_rv {
                        self.emit(Event::Updated { old, new });
                    }
                }
            }
        }
        let stale: Vec<String> = self.store.keys().into_iter().filter(|k| !seen.contains(k)).collect();
        for k in stale {
            if let Some(o) = self.store.remove(&k) {
                self.emit(Event::Deleted(o));
            }
        }
        let _ = self.synced_tx.send(true);
        Ok(rv)
    }

    async fn run(&self) {
        let mut backoff = Duration::from_millis(200);
        loop {
            let mut rv = match self.relist().await {
                Ok(rv) => {
                    backoff = Duration::from_millis(200);
                    rv
                }
                Err(e) => {
                    tracing::warn!(kind = %self.name, "list failed: {e}; retrying in {backoff:?}");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    continue;
                }
            };
            // Watch until the stream breaks; resume from the last seen RV, relist on 410.
            'watch: loop {
                let timeout = 300 + (rv.len() as u32 % 60);
                let mut stream = match self.lw.watch(&rv, timeout).await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(kind = %self.name, "watch failed: {e}");
                        tokio::time::sleep(backoff).await;
                        break 'watch;
                    }
                };
                while let Some(ev) = stream.next().await {
                    match ev {
                        Ok((type_, obj)) => {
                            if let Some(new_rv) = obj.resource_version() {
                                rv = new_rv.to_string();
                            }
                            match type_ {
                                EventType::Added | EventType::Modified => match self.store.upsert(obj) {
                                    (new, None) => self.emit(Event::Added(new)),
                                    (new, Some(old)) => self.emit(Event::Updated { old, new }),
                                },
                                EventType::Deleted => {
                                    let key = obj.cache_key();
                                    let obj = self.store.remove(&key).unwrap_or_else(|| Arc::new(obj));
                                    self.emit(Event::Deleted(obj));
                                }
                                EventType::Bookmark | EventType::Error => {}
                            }
                        }
                        Err(e) if e.is_gone() => {
                            tracing::info!(kind = %self.name, "watch expired (410), relisting");
                            break 'watch;
                        }
                        Err(e) => {
                            tracing::warn!(kind = %self.name, "watch error: {e}");
                            tokio::time::sleep(backoff).await;
                            continue 'watch;
                        }
                    }
                }
                // Stream ended normally (server timeout): re-watch from rv.
            }
        }
    }
}
