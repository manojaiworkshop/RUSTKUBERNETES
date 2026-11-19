//! etcd v3 implementation of [`Storage`] (`k8s.io/apiserver/pkg/storage/etcd3`),
//! over gRPC (`clientv3`), as the Kubernetes API server does it:
//!
//! * **create**: `Txn(If(CreateRevision(key) == 0), Then(Put(key, data)))`;
//!   an unsuccessful txn is `AlreadyExists`. resourceVersion = the txn header revision.
//! * **get / list**: `Range`; an object's resourceVersion is its `ModRevision`,
//!   a list's is the response header revision.
//! * **update** (`GuaranteedUpdate`): `Txn(If(ModRevision(key) == rv), Then(Put), Else(Get))`;
//!   without a precondition the key must exist (`CreateRevision > 0`).
//! * **delete**: `Txn(If(ModRevision == rv), Then(Delete(prevKV)), Else(Get))`; the
//!   returned object carries the deletion revision.
//! * **watch**: `Watch(prefix, WithRev(rv+1), WithPrevKV)`; `CreateRevision == ModRevision`
//!   → ADDED, other PUTs → MODIFIED, DELETE → the previous value; a compacted
//!   revision is `410 Gone`.
//! * **compaction** (`compact.go`): every interval, a `compact_rev_key` txn elects one
//!   compactor among API servers, which compacts to the revision seen one interval ago.
//! * objects are stored without `resourceVersion` (`PrepareObjectForStorage`), passed
//!   through the value transformer (encryption at rest) with the etcd key as context.
//!
//! Storage media type: JSON (`--storage-media-type=application/json`); upstream's
//! default is protobuf.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use etcd_clientv3::{
    cmp_create_revision, cmp_mod_revision, cmp_version, compare::CompareResult, op_delete, op_get, op_put, prefix_range_end, response_op,
    Client, EventType as EtcdEvent, KeyValue, RangeRequest, TxnRequest, TxnResponse, WatchCreateRequest,
};
use futures::StreamExt;
use k8s_apimachinery::watch::EventType;
use serde_json::{json, Value};

use super::value::Transformer;
use super::{Revision, Storage, StorageError, StoredEvent, WatchStream};

/// The key all API servers use to elect the compactor (`compactRevKey`).
const COMPACT_REV_KEY: &[u8] = b"compact_rev_key";

/// `--etcd-*` flags.
#[derive(Clone, Default)]
pub struct EtcdConfig {
    /// `--etcd-servers`
    pub endpoints: Vec<String>,
    /// `--etcd-cafile`, `--etcd-certfile`, `--etcd-keyfile` contents.
    pub ca: Option<Vec<u8>>,
    pub cert: Option<Vec<u8>>,
    pub key: Option<Vec<u8>>,
    /// `--etcd-prefix` (default `/registry`).
    pub prefix: String,
    /// `--etcd-compaction-interval` (default 5m; `None` disables).
    pub compaction_interval: Option<Duration>,
    /// `--encryption-provider-config`
    pub transformer: Option<Arc<Transformer>>,
}

pub struct EtcdStore {
    client: Client,
    prefix: String,
    revision: Arc<AtomicU64>,
    transformer: Option<Arc<Transformer>>,
}

fn internal(e: impl std::fmt::Display) -> StorageError {
    StorageError::Internal(e.to_string())
}

fn set_rv(obj: &mut Value, rev: Revision) {
    if let Some(m) = obj.get_mut("metadata").and_then(Value::as_object_mut) {
        m.insert("resourceVersion".into(), json!(rev.to_string()));
    }
}

/// Shared by the store and its watch streams.
#[derive(Clone)]
struct Codec {
    prefix: String,
    transformer: Option<Arc<Transformer>>,
}

impl Codec {
    /// API key (`/registry/...`) → etcd key (`--etcd-prefix` applied).
    fn key(&self, key: &str) -> Vec<u8> {
        match key.strip_prefix("/registry") {
            Some(rest) => format!("{}{rest}", self.prefix).into_bytes(),
            None => key.as_bytes().to_vec(),
        }
    }

    fn api_key(&self, k: &[u8]) -> String {
        let k = String::from_utf8_lossy(k);
        match k.strip_prefix(&self.prefix) {
            Some(rest) => format!("/registry{rest}"),
            None => k.into_owned(),
        }
    }

    /// `PrepareObjectForStorage` + transformer.
    fn encode(&self, obj: &Value, etcd_key: &[u8]) -> Result<Vec<u8>, StorageError> {
        let mut o = obj.clone();
        if let Some(m) = o.get_mut("metadata").and_then(Value::as_object_mut) {
            m.remove("resourceVersion");
            m.remove("selfLink");
        }
        let data = serde_json::to_vec(&o).map_err(internal)?;
        match &self.transformer {
            Some(t) => t.to_storage(&data, etcd_key, &self.api_key(etcd_key)).map_err(internal),
            None => Ok(data),
        }
    }

    fn decode(&self, data: &[u8], etcd_key: &[u8], rev: Revision) -> Result<Value, StorageError> {
        let plain = match &self.transformer {
            Some(t) => t
                .from_storage(data, etcd_key, &self.api_key(etcd_key))
                .map_err(|e| internal(format!("unable to transform key {:?}: {e}", String::from_utf8_lossy(etcd_key))))?,
            None => data.to_vec(),
        };
        let mut v: Value = serde_json::from_slice(&plain)
            .map_err(|e| internal(format!("corrupt object in etcd at {:?}: {e}", String::from_utf8_lossy(etcd_key))))?;
        set_rv(&mut v, rev);
        Ok(v)
    }

    fn kv(&self, kv: &KeyValue) -> Result<Value, StorageError> {
        self.decode(&kv.value, &kv.key, kv.mod_revision as Revision)
    }

    fn event(&self, ev: &etcd_clientv3::Event) -> Result<StoredEvent, StorageError> {
        let kv = ev.kv.as_ref().ok_or_else(|| internal("watch event without kv"))?;
        let rev = kv.mod_revision as Revision;
        let prev = match &ev.prev_kv {
            Some(p) => Some(self.kv(p)?),
            None => None,
        };
        if ev.r#type == EtcdEvent::Delete as i32 {
            let mut obj = prev.clone().ok_or_else(|| internal("etcd event without prevKV (watch must use WithPrevKV)"))?;
            set_rv(&mut obj, rev);
            return Ok(StoredEvent { type_: EventType::Deleted, key: self.api_key(&kv.key), object: obj, prev, revision: rev });
        }
        let type_ = if kv.create_revision == kv.mod_revision { EventType::Added } else { EventType::Modified };
        Ok(StoredEvent { type_, key: self.api_key(&kv.key), object: self.kv(kv)?, prev, revision: rev })
    }
}

fn grpc(e: etcd_clientv3::Error) -> StorageError {
    if e.message().contains("required revision has been compacted") {
        return StorageError::Gone { requested: 0, oldest: 0 };
    }
    StorageError::Internal(format!("etcd: {}", e.message()))
}

/// The `Range` response inside a failed txn's `Else(Get)`.
fn else_range(r: &TxnResponse) -> Option<&KeyValue> {
    match r.responses.first()?.response.as_ref()? {
        response_op::Response::ResponseRange(rr) => rr.kvs.first(),
        _ => None,
    }
}

impl EtcdStore {
    /// Connects, checks health (`Maintenance.Status`) and starts the compactor.
    pub async fn connect(cfg: EtcdConfig) -> Result<Arc<Self>, StorageError> {
        if cfg.endpoints.is_empty() {
            return Err(internal("--etcd-servers is empty"));
        }
        let tls = (cfg.ca.is_some() || cfg.cert.is_some()).then(|| etcd_clientv3::Tls {
            ca: cfg.ca.clone(),
            cert: cfg.cert.clone(),
            key: cfg.key.clone(),
        });
        let client = Client::connect(&cfg.endpoints, tls, Duration::from_secs(5)).await.map_err(internal)?;
        let store = Arc::new(Self {
            client,
            prefix: if cfg.prefix.is_empty() { "/registry".into() } else { cfg.prefix.trim_end_matches('/').to_string() },
            revision: Arc::new(AtomicU64::new(0)),
            transformer: cfg.transformer.clone(),
        });
        let mut last = String::new();
        for _ in 0..60 {
            match store.client.status().await {
                Ok(h) => {
                    store.observe(h.revision);
                    last.clear();
                    break;
                }
                Err(e) => last = e.message().to_string(),
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if !last.is_empty() {
            return Err(internal(format!("cannot reach etcd at {}: {last}", cfg.endpoints.join(","))));
        }
        if let Some(every) = cfg.compaction_interval {
            let c = store.client.clone();
            let rev = store.revision.clone();
            tokio::spawn(compactor(c, rev, every));
        }
        tracing::info!(servers = %cfg.endpoints.join(","), prefix = %store.prefix, revision = store.current_revision(),
            encryption = store.transformer.is_some(), "connected to etcd");
        Ok(store)
    }

    fn codec(&self) -> Codec {
        Codec { prefix: self.prefix.clone(), transformer: self.transformer.clone() }
    }

    fn observe(&self, rev: i64) {
        self.revision.fetch_max(rev.max(0) as u64, Ordering::SeqCst);
    }

    async fn txn(&self, req: TxnRequest) -> Result<TxnResponse, StorageError> {
        let r = self.client.txn(req).await.map_err(grpc)?;
        self.observe(r.header.as_ref().map(|h| h.revision).unwrap_or(0));
        Ok(r)
    }

    async fn range(&self, req: RangeRequest) -> Result<etcd_clientv3::RangeResponse, StorageError> {
        let r = self.client.range(req).await.map_err(grpc)?;
        self.observe(r.header.as_ref().map(|h| h.revision).unwrap_or(0));
        Ok(r)
    }

    /// Fresh current revision (a cheap count-only range).
    async fn fetch_revision(&self) -> Result<Revision, StorageError> {
        let r = self.range(RangeRequest { key: COMPACT_REV_KEY.to_vec(), count_only: true, ..Default::default() }).await?;
        Ok(r.header.map(|h| h.revision).unwrap_or(0) as Revision)
    }
}

/// `compact.go`: one compactor among all API servers, elected through `compact_rev_key`.
async fn compactor(client: Client, revision: Arc<AtomicU64>, every: Duration) {
    let (mut t, mut rev) = (0i64, 0i64);
    loop {
        tokio::time::sleep(every).await;
        let txn = TxnRequest {
            compare: vec![cmp_version(COMPACT_REV_KEY.to_vec(), CompareResult::Equal, t)],
            success: vec![op_put(COMPACT_REV_KEY.to_vec(), vec![])],
            failure: vec![op_get(COMPACT_REV_KEY.to_vec())],
        };
        match client.txn(txn).await {
            Ok(r) => {
                let cur_rev = r.header.as_ref().map(|h| h.revision).unwrap_or(0);
                revision.fetch_max(cur_rev.max(0) as u64, Ordering::SeqCst);
                if !r.succeeded {
                    // Another API server compacted: follow its version.
                    t = else_range(&r).map(|kv| kv.version).unwrap_or(t);
                    rev = cur_rev;
                    continue;
                }
                t += 1;
                if rev > 0 {
                    match client.compact(rev).await {
                        Ok(()) => tracing::info!(revision = rev, "compacted etcd"),
                        Err(e) => tracing::warn!("etcd compaction: {}", e.message()),
                    }
                }
                rev = cur_rev;
            }
            Err(e) => tracing::debug!("etcd compactor: {}", e.message()),
        }
    }
}

#[async_trait]
impl Storage for EtcdStore {
    async fn create(&self, key: &str, mut obj: Value) -> Result<Value, StorageError> {
        let c = self.codec();
        let k = c.key(key);
        let data = c.encode(&obj, &k)?;
        let r = self
            .txn(TxnRequest {
                compare: vec![cmp_create_revision(k.clone(), CompareResult::Equal, 0)],
                success: vec![op_put(k, data)],
                failure: vec![],
            })
            .await?;
        if !r.succeeded {
            return Err(StorageError::AlreadyExists(key.into()));
        }
        set_rv(&mut obj, r.header.map(|h| h.revision).unwrap_or(0) as Revision);
        Ok(obj)
    }

    async fn get(&self, key: &str) -> Result<Value, StorageError> {
        let c = self.codec();
        let r = self.range(RangeRequest { key: c.key(key), ..Default::default() }).await?;
        let kv = r.kvs.first().ok_or_else(|| StorageError::NotFound(key.into()))?;
        c.kv(kv)
    }

    async fn list(&self, prefix: &str) -> Result<(Vec<Value>, Revision), StorageError> {
        let c = self.codec();
        let k = c.key(prefix);
        let r = self.range(RangeRequest { range_end: prefix_range_end(&k), key: k, ..Default::default() }).await?;
        let items = r.kvs.iter().map(|kv| c.kv(kv)).collect::<Result<Vec<_>, _>>()?;
        Ok((items, r.header.map(|h| h.revision).unwrap_or(0) as Revision))
    }

    async fn update(&self, key: &str, expected: Option<Revision>, mut obj: Value) -> Result<Value, StorageError> {
        let c = self.codec();
        let k = c.key(key);
        let data = c.encode(&obj, &k)?;
        let cmp = match expected {
            Some(rv) => cmp_mod_revision(k.clone(), CompareResult::Equal, rv as i64),
            None => cmp_create_revision(k.clone(), CompareResult::Greater, 0),
        };
        let r = self.txn(TxnRequest { compare: vec![cmp], success: vec![op_put(k.clone(), data)], failure: vec![op_get(k)] }).await?;
        if !r.succeeded {
            return Err(if else_range(&r).is_some() { StorageError::Conflict(key.into()) } else { StorageError::NotFound(key.into()) });
        }
        set_rv(&mut obj, r.header.map(|h| h.revision).unwrap_or(0) as Revision);
        Ok(obj)
    }

    async fn delete(&self, key: &str, expected: Option<Revision>) -> Result<Value, StorageError> {
        let c = self.codec();
        let k = c.key(key);
        let cmp = match expected {
            Some(rv) => cmp_mod_revision(k.clone(), CompareResult::Equal, rv as i64),
            None => cmp_create_revision(k.clone(), CompareResult::Greater, 0),
        };
        let r = self.txn(TxnRequest { compare: vec![cmp], success: vec![op_delete(k.clone())], failure: vec![op_get(k.clone())] }).await?;
        if !r.succeeded {
            return Err(if else_range(&r).is_some() { StorageError::Conflict(key.into()) } else { StorageError::NotFound(key.into()) });
        }
        let rev = r.header.as_ref().map(|h| h.revision).unwrap_or(0) as Revision;
        let prev = match r.responses.first().and_then(|x| x.response.as_ref()) {
            Some(response_op::Response::ResponseDeleteRange(d)) => d.prev_kvs.first().cloned(),
            _ => None,
        };
        let prev = prev.ok_or_else(|| StorageError::NotFound(key.into()))?;
        c.decode(&prev.value, &prev.key, rev)
    }

    async fn watch(&self, prefix: &str, from: Revision) -> Result<WatchStream, StorageError> {
        let current = self.fetch_revision().await?;
        if from > current {
            // A revision this etcd never issued: make the client relist.
            return Err(StorageError::Gone { requested: from, oldest: current });
        }
        let c = self.codec();
        let k = c.key(prefix);
        let mut stream = self
            .client
            .watch(WatchCreateRequest {
                range_end: prefix_range_end(&k),
                key: k,
                start_revision: from as i64 + 1,
                prev_kv: true,
                ..Default::default()
            })
            .await
            .map_err(grpc)?;
        // The first response confirms the watch or reports a compacted revision.
        let first = stream.message().await.map_err(grpc)?.ok_or_else(|| internal("etcd closed the watch"))?;
        if first.canceled || first.compact_revision > 0 {
            return Err(StorageError::Gone { requested: from, oldest: first.compact_revision.max(0) as Revision });
        }
        let pending: Vec<StoredEvent> = first.events.iter().map(|e| c.event(e)).collect::<Result<_, _>>()?;
        let revision = self.revision.clone();
        let out = async_stream::stream! {
            for e in pending {
                yield Ok(e);
            }
            let mut last = from;
            loop {
                match stream.message().await {
                    Ok(Some(resp)) => {
                        if resp.canceled || resp.compact_revision > 0 {
                            yield Err(StorageError::Gone { requested: last, oldest: resp.compact_revision.max(0) as Revision });
                            return;
                        }
                        if let Some(h) = &resp.header {
                            revision.fetch_max(h.revision.max(0) as u64, Ordering::SeqCst);
                        }
                        for e in &resp.events {
                            match c.event(e) {
                                Ok(ev) => {
                                    last = ev.revision;
                                    yield Ok(ev);
                                }
                                Err(err) => {
                                    yield Err(err);
                                    return;
                                }
                            }
                        }
                    }
                    Ok(None) => return,
                    Err(e) => {
                        yield Err(grpc(e));
                        return;
                    }
                }
            }
        };
        Ok(Box::pin(out.boxed()))
    }

    fn current_revision(&self) -> Revision {
        self.revision.load(Ordering::SeqCst)
    }

    async fn check(&self) -> Result<(), StorageError> {
        self.client.status().await.map(|_| ()).map_err(grpc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the storage conformance suite against a real etcd when
    /// `ETCD_TEST_ENDPOINT` is set (hack/test-etcd.sh starts one).
    #[tokio::test]
    async fn conformance_against_etcd() {
        let Ok(ep) = std::env::var("ETCD_TEST_ENDPOINT") else { return };
        let prefix = format!("/test-{}-{}", std::process::id(), chrono::Utc::now().timestamp_micros());
        let s = EtcdStore::connect(EtcdConfig { endpoints: vec![ep], prefix, ..Default::default() }).await.unwrap();
        super::super::conformance::run(s).await;
    }

    #[tokio::test]
    async fn encrypted_values_in_etcd() {
        let Ok(ep) = std::env::var("ETCD_TEST_ENDPOINT") else { return };
        let prefix = format!("/test-enc-{}", std::process::id());
        let t = Arc::new(Transformer::from_yaml(super::super::value::tests::CONFIG).unwrap());
        let s = EtcdStore::connect(EtcdConfig { endpoints: vec![ep], prefix: prefix.clone(), transformer: Some(t), ..Default::default() })
            .await
            .unwrap();
        s.create("/registry/secrets/ns/s1", json!({"metadata": {"name": "s1"}, "data": {"k": "c2VjcmV0"}})).await.unwrap();
        let raw = s.range(RangeRequest { key: format!("{prefix}/secrets/ns/s1").into_bytes(), ..Default::default() }).await.unwrap();
        let stored = &raw.kvs[0].value;
        assert!(stored.starts_with(b"k8s:enc:aesgcm:v1:key1:"), "secret is encrypted at rest");
        assert!(!String::from_utf8_lossy(stored).contains("c2VjcmV0"));
        assert_eq!(s.get("/registry/secrets/ns/s1").await.unwrap()["data"]["k"], "c2VjcmV0");
        s.create("/registry/configmaps/ns/c1", json!({"metadata": {"name": "c1"}})).await.unwrap();
        let raw = s.range(RangeRequest { key: format!("{prefix}/configmaps/ns/c1").into_bytes(), ..Default::default() }).await.unwrap();
        assert!(raw.kvs[0].value.starts_with(b"{"), "configmaps are not in the encryption config");
    }
}
