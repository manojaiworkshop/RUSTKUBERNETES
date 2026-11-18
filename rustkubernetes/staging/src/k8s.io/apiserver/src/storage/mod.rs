//! Port of `k8s.io/apiserver/pkg/storage`.
//!
//! The [`Storage`] trait is the equivalent of `storage.Interface`: a
//! versioned key/value store where every write bumps a global revision
//! (like etcd's `ModRevision`), which becomes the object's
//! `metadata.resourceVersion`. Watches deliver ordered events after a given
//! revision.
//!
//! Keys follow the etcd layout of the real API server:
//! `/registry/<group-resource>/<namespace>/<name>` (or without namespace for
//! cluster-scoped resources).

pub mod memory;

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use k8s_apimachinery::watch::EventType;
use serde_json::Value;

pub type Revision = u64;

/// One change recorded by the store.
#[derive(Clone, Debug)]
pub struct StoredEvent {
    pub type_: EventType,
    pub key: String,
    /// The object after the change (for DELETED: the last state, with the delete revision).
    pub object: Value,
    /// The object before the change (MODIFIED / DELETED).
    pub prev: Option<Value>,
    pub revision: Revision,
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum StorageError {
    #[error("key not found: {0}")]
    NotFound(String),
    #[error("key already exists: {0}")]
    AlreadyExists(String),
    #[error("resource version conflict on {0}")]
    Conflict(String),
    #[error("too old resource version: {requested} ({oldest})")]
    Gone { requested: Revision, oldest: Revision },
    #[error("storage error: {0}")]
    Internal(String),
}

pub type WatchStream = Pin<Box<dyn Stream<Item = Result<StoredEvent, StorageError>> + Send>>;

/// `storage.Interface`
#[async_trait]
pub trait Storage: Send + Sync {
    /// Persists pending state (no-op for stores without persistence).
    fn flush(&self) -> Result<(), StorageError> {
        Ok(())
    }

    /// Creates a new object. Fails with `AlreadyExists`.
    async fn create(&self, key: &str, obj: Value) -> Result<Value, StorageError>;

    async fn get(&self, key: &str) -> Result<Value, StorageError>;

    /// Lists all objects whose key starts with `prefix`, sorted by key, plus the current revision.
    async fn list(&self, prefix: &str) -> Result<(Vec<Value>, Revision), StorageError>;

    /// Replaces an object. `expected` is the resourceVersion precondition
    /// (`None` = unconditional). Fails with `NotFound` / `Conflict`.
    async fn update(&self, key: &str, expected: Option<Revision>, obj: Value) -> Result<Value, StorageError>;

    /// Deletes an object, returning it with the deletion revision as resourceVersion.
    async fn delete(&self, key: &str, expected: Option<Revision>) -> Result<Value, StorageError>;

    /// Streams events for keys under `prefix` with revision > `from`.
    /// Fails with `Gone` if `from` is older than the retained history.
    async fn watch(&self, prefix: &str, from: Revision) -> Result<WatchStream, StorageError>;

    fn current_revision(&self) -> Revision;
}

/// Parses a `resourceVersion` string. Empty → `None`.
pub fn parse_rv(rv: &str) -> Result<Option<Revision>, String> {
    if rv.is_empty() {
        return Ok(None);
    }
    rv.parse::<Revision>().map(Some).map_err(|_| format!("invalid resource version: {rv:?}"))
}
