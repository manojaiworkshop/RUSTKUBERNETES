//! Version conversion between the served and the stored `apiVersion` of a
//! resource (`runtime.ObjectConvertor` + `UnsafeConvertToVersion` in the
//! storage codec). Multi-version custom resources are stored in their storage
//! version and converted on every read and write.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;

use super::{Revision, Storage, StorageError, StoredEvent, WatchStream};

/// Converts objects between versions of one resource.
#[async_trait]
pub trait Converter: Send + Sync {
    /// Returns `obj` converted to `api_version`.
    async fn convert(&self, obj: Value, api_version: &str) -> Result<Value, String>;
}

/// The `None` conversion strategy: only `apiVersion` changes.
pub struct NoneConverter;

#[async_trait]
impl Converter for NoneConverter {
    async fn convert(&self, mut obj: Value, api_version: &str) -> Result<Value, String> {
        if let Some(o) = obj.as_object_mut() {
            o.insert("apiVersion".into(), Value::String(api_version.to_string()));
        }
        Ok(obj)
    }
}

/// Wraps a store so that callers only see `served` objects while `stored` ones are persisted.
pub struct ConvertingStorage {
    pub inner: Arc<dyn Storage>,
    pub converter: Arc<dyn Converter>,
    pub served: String,
    pub stored: String,
}

impl ConvertingStorage {
    async fn out(&self, obj: Value) -> Result<Value, StorageError> {
        convert(&*self.converter, obj, &self.served).await
    }

    async fn to_storage(&self, obj: Value) -> Result<Value, StorageError> {
        convert(&*self.converter, obj, &self.stored).await
    }
}

async fn convert(c: &dyn Converter, obj: Value, to: &str) -> Result<Value, StorageError> {
    if obj.get("apiVersion").and_then(Value::as_str) == Some(to) {
        return Ok(obj);
    }
    c.convert(obj, to).await.map_err(StorageError::Internal)
}

#[async_trait]
impl Storage for ConvertingStorage {
    fn flush(&self) -> Result<(), StorageError> {
        self.inner.flush()
    }

    async fn create(&self, key: &str, obj: Value) -> Result<Value, StorageError> {
        let obj = self.to_storage(obj).await?;
        let created = self.inner.create(key, obj).await?;
        self.out(created).await
    }

    async fn get(&self, key: &str) -> Result<Value, StorageError> {
        let obj = self.inner.get(key).await?;
        self.out(obj).await
    }

    async fn list(&self, prefix: &str) -> Result<(Vec<Value>, Revision), StorageError> {
        let (items, rev) = self.inner.list(prefix).await?;
        let mut out = Vec::with_capacity(items.len());
        for i in items {
            out.push(self.out(i).await?);
        }
        Ok((out, rev))
    }

    async fn update(&self, key: &str, expected: Option<Revision>, obj: Value) -> Result<Value, StorageError> {
        let obj = self.to_storage(obj).await?;
        let updated = self.inner.update(key, expected, obj).await?;
        self.out(updated).await
    }

    async fn delete(&self, key: &str, expected: Option<Revision>) -> Result<Value, StorageError> {
        let deleted = self.inner.delete(key, expected).await?;
        self.out(deleted).await
    }

    async fn watch(&self, prefix: &str, from: Revision) -> Result<WatchStream, StorageError> {
        let inner = self.inner.watch(prefix, from).await?;
        let (conv, served) = (self.converter.clone(), self.served.clone());
        Ok(Box::pin(inner.then(move |ev| {
            let (conv, served) = (conv.clone(), served.clone());
            async move {
                let ev = ev?;
                let object = convert(&*conv, ev.object, &served).await?;
                let prev = match ev.prev {
                    Some(p) => Some(convert(&*conv, p, &served).await?),
                    None => None,
                };
                Ok(StoredEvent { type_: ev.type_, key: ev.key, object, prev, revision: ev.revision })
            }
        })))
    }

    fn current_revision(&self) -> Revision {
        self.inner.current_revision()
    }

    async fn check(&self) -> Result<(), StorageError> {
        self.inner.check().await
    }
}
