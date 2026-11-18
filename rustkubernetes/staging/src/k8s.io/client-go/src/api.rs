//! Typed and dynamic resource clients (client-go `kubernetes/typed/*` and `dynamic`).

use std::marker::PhantomData;

use futures::{Stream, StreamExt};
use k8s_api::Resource;
use k8s_apimachinery::errors::{ApiError, Status};
use k8s_apimachinery::meta::List;
use k8s_apimachinery::watch::{EventType, WatchEvent};
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::rest::{Client, Error};

/// `metav1.ListOptions` (subset).
#[derive(Clone, Debug, Default)]
pub struct ListParams {
    pub label_selector: Option<String>,
    pub field_selector: Option<String>,
    pub limit: Option<u32>,
    pub continue_token: Option<String>,
    pub resource_version: Option<String>,
    pub timeout_seconds: Option<u32>,
}

impl ListParams {
    pub fn labels(mut self, s: &str) -> Self {
        self.label_selector = Some(s.into());
        self
    }
    pub fn fields(mut self, s: &str) -> Self {
        self.field_selector = Some(s.into());
        self
    }
    pub fn limit(mut self, l: u32) -> Self {
        self.limit = Some(l);
        self
    }

    fn query(&self, watch: bool) -> String {
        let mut q: Vec<(String, String)> = vec![];
        if let Some(s) = &self.label_selector {
            q.push(("labelSelector".into(), s.clone()));
        }
        if let Some(s) = &self.field_selector {
            q.push(("fieldSelector".into(), s.clone()));
        }
        if let Some(l) = self.limit {
            q.push(("limit".into(), l.to_string()));
        }
        if let Some(c) = &self.continue_token {
            q.push(("continue".into(), c.clone()));
        }
        if let Some(rv) = &self.resource_version {
            q.push(("resourceVersion".into(), rv.clone()));
        }
        if let Some(t) = self.timeout_seconds {
            q.push(("timeoutSeconds".into(), t.to_string()));
        }
        if watch {
            q.push(("watch".into(), "true".into()));
            q.push(("allowWatchBookmarks".into(), "true".into()));
        }
        encode_query(&q)
    }
}

fn encode_query(q: &[(String, String)]) -> String {
    if q.is_empty() {
        return String::new();
    }
    let enc = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect::<String>()
    };
    format!("?{}", q.iter().map(|(k, v)| format!("{k}={}", enc(v))).collect::<Vec<_>>().join("&"))
}

/// Patch content types (`types.PatchType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchType {
    Json,
    Merge,
    Strategic,
    Apply,
}

impl PatchType {
    pub fn content_type(&self) -> &'static str {
        match self {
            PatchType::Json => "application/json-patch+json",
            PatchType::Merge => "application/merge-patch+json",
            PatchType::Strategic => "application/strategic-merge-patch+json",
            PatchType::Apply => "application/apply-patch+yaml",
        }
    }
}

/// `metav1.DeleteOptions` (subset).
#[derive(Clone, Debug, Default)]
pub struct DeleteParams {
    pub grace_period_seconds: Option<i64>,
    /// `Background`, `Foreground` or `Orphan`.
    pub propagation_policy: Option<String>,
    pub precondition_uid: Option<String>,
    pub precondition_resource_version: Option<String>,
}

impl DeleteParams {
    pub fn background() -> Self {
        Self { propagation_policy: Some("Background".into()), ..Default::default() }
    }
    pub fn foreground() -> Self {
        Self { propagation_policy: Some("Foreground".into()), ..Default::default() }
    }
    pub fn orphan() -> Self {
        Self { propagation_policy: Some("Orphan".into()), ..Default::default() }
    }

    fn to_json(&self) -> Value {
        let mut o = json!({"kind": "DeleteOptions", "apiVersion": "v1"});
        if let Some(g) = self.grace_period_seconds {
            o["gracePeriodSeconds"] = json!(g);
        }
        if let Some(p) = &self.propagation_policy {
            o["propagationPolicy"] = json!(p);
        }
        if let Some(u) = &self.precondition_uid {
            o["preconditions"]["uid"] = json!(u);
        }
        if let Some(rv) = &self.precondition_resource_version {
            o["preconditions"]["resourceVersion"] = json!(rv);
        }
        o
    }
}

/// Describes where a resource lives (used by the dynamic client).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiResource {
    pub group: String,
    pub version: String,
    pub plural: String,
    pub kind: String,
    pub namespaced: bool,
}

impl ApiResource {
    /// `v1` or `apps/v1`
    pub fn api_version(&self) -> String {
        if self.group.is_empty() {
            self.version.clone()
        } else {
            format!("{}/{}", self.group, self.version)
        }
    }

    pub fn of<K: Resource>() -> Self {
        Self {
            group: K::GROUP.into(),
            version: K::VERSION.into(),
            plural: K::PLURAL.into(),
            kind: K::KIND.into(),
            namespaced: K::NAMESPACED,
        }
    }

    /// `/api/v1/namespaces/<ns>/pods` or `/apis/apps/v1/deployments`.
    pub fn collection_path(&self, namespace: Option<&str>) -> String {
        let base = if self.group.is_empty() { format!("/api/{}", self.version) } else { format!("/apis/{}/{}", self.group, self.version) };
        match namespace.filter(|n| self.namespaced && !n.is_empty()) {
            Some(ns) => format!("{base}/namespaces/{ns}/{}", self.plural),
            None => format!("{base}/{}", self.plural),
        }
    }
}

/// Untyped client for any resource (`dynamic.Interface`).
#[derive(Clone, Debug)]
pub struct DynamicApi {
    client: Client,
    resource: ApiResource,
    namespace: Option<String>,
}

impl DynamicApi {
    pub fn new(client: Client, resource: ApiResource, namespace: Option<&str>) -> Self {
        Self { client, resource, namespace: namespace.map(String::from) }
    }

    pub fn resource(&self) -> &ApiResource {
        &self.resource
    }

    fn collection(&self) -> String {
        self.resource.collection_path(self.namespace.as_deref())
    }

    fn item(&self, name: &str, sub: Option<&str>) -> String {
        match sub {
            Some(s) => format!("{}/{name}/{s}", self.collection()),
            None => format!("{}/{name}", self.collection()),
        }
    }

    pub async fn get(&self, name: &str) -> Result<Value, Error> {
        self.client.get_json(&self.item(name, None)).await
    }

    pub async fn list(&self, lp: &ListParams) -> Result<Value, Error> {
        self.client.get_json(&format!("{}{}", self.collection(), lp.query(false))).await
    }

    pub async fn create(&self, obj: &Value) -> Result<Value, Error> {
        self.client.send_json(Method::POST, &self.collection(), obj).await
    }

    pub async fn replace(&self, name: &str, obj: &Value) -> Result<Value, Error> {
        self.client.send_json(Method::PUT, &self.item(name, None), obj).await
    }

    pub async fn replace_status(&self, name: &str, obj: &Value) -> Result<Value, Error> {
        self.client.send_json(Method::PUT, &self.item(name, Some("status")), obj).await
    }

    pub async fn patch(&self, name: &str, pt: PatchType, patch: &Value) -> Result<Value, Error> {
        self.patch_sub(name, None, pt, patch).await
    }

    pub async fn patch_status(&self, name: &str, pt: PatchType, patch: &Value) -> Result<Value, Error> {
        self.patch_sub(name, Some("status"), pt, patch).await
    }

    pub async fn patch_sub(&self, name: &str, sub: Option<&str>, pt: PatchType, patch: &Value) -> Result<Value, Error> {
        let body = match pt {
            PatchType::Apply => serde_yaml::to_string(patch).map_err(|e| Error::Decode(e.to_string()))?.into_bytes(),
            _ => serde_json::to_vec(patch).unwrap(),
        };
        let path = format!("{}?fieldManager=rustkubernetes", self.item(name, sub));
        self.client.request(Method::PATCH, &path, Some((pt.content_type(), body))).await
    }

    /// Deletes an object. `grace_period_seconds` = `Some(0)` forces immediate deletion.
    pub async fn delete(&self, name: &str, grace_period_seconds: Option<i64>) -> Result<Value, Error> {
        self.delete_with(name, &DeleteParams { grace_period_seconds, ..Default::default() }).await
    }

    /// Deletes with full `DeleteOptions` (propagation policy, preconditions).
    pub async fn delete_with(&self, name: &str, dp: &DeleteParams) -> Result<Value, Error> {
        self.client.send_json(Method::DELETE, &self.item(name, None), &dp.to_json()).await
    }

    /// `DELETE` on the collection (`deletecollection`).
    pub async fn delete_collection(&self, lp: &ListParams, dp: &DeleteParams) -> Result<Value, Error> {
        self.client.send_json(Method::DELETE, &format!("{}{}", self.collection(), lp.query(false)), &dp.to_json()).await
    }

    /// `PUT` to a subresource (e.g. `namespaces/<n>/finalize`).
    pub async fn replace_sub(&self, name: &str, sub: &str, obj: &Value) -> Result<Value, Error> {
        self.client.send_json(Method::PUT, &self.item(name, Some(sub)), obj).await
    }

    /// POST to a subresource (`binding`, `eviction`).
    pub async fn create_subresource(&self, name: &str, sub: &str, body: &Value) -> Result<Value, Error> {
        self.client.send_json(Method::POST, &self.item(name, Some(sub)), body).await
    }

    /// Streams watch events (`?watch=true`). Server-side `ERROR` events become `Err(Error::Api)`.
    pub async fn watch(&self, lp: &ListParams) -> Result<impl Stream<Item = Result<WatchEvent<Value>, Error>> + use<>, Error> {
        let lines = self.client.watch_lines(&format!("{}{}", self.collection(), lp.query(true))).await?;
        Ok(lines.map(|line| {
            let ev: WatchEvent<Value> = serde_json::from_value(line?).map_err(|e| Error::Decode(e.to_string()))?;
            if ev.type_ == EventType::Error {
                let status: Status = serde_json::from_value(ev.object).unwrap_or_default();
                return Err(Error::Api(ApiError::from_status(&status)));
            }
            Ok(ev)
        }))
    }
}

/// Typed client for one resource kind (`CoreV1().Pods(ns)`).
#[derive(Clone, Debug)]
pub struct Api<K> {
    inner: DynamicApi,
    _k: PhantomData<K>,
}

fn decode<K: DeserializeOwned>(v: Value) -> Result<K, Error> {
    serde_json::from_value(v).map_err(|e| Error::Decode(e.to_string()))
}

impl<K: Resource> Api<K> {
    /// Namespaced resource in one namespace.
    pub fn namespaced(client: Client, namespace: &str) -> Self {
        Self { inner: DynamicApi::new(client, ApiResource::of::<K>(), Some(namespace)), _k: PhantomData }
    }

    /// Cluster-scoped resource, or a namespaced resource across all namespaces.
    pub fn all(client: Client) -> Self {
        Self { inner: DynamicApi::new(client, ApiResource::of::<K>(), None), _k: PhantomData }
    }

    pub fn dynamic(&self) -> &DynamicApi {
        &self.inner
    }

    /// The underlying client (to build other APIs).
    pub fn dynamic_client(&self) -> Client {
        self.inner.client.clone()
    }

    pub async fn get(&self, name: &str) -> Result<K, Error> {
        decode(self.inner.get(name).await?)
    }

    /// Returns `None` instead of a NotFound error.
    pub async fn get_opt(&self, name: &str) -> Result<Option<K>, Error> {
        match self.get(name).await {
            Ok(o) => Ok(Some(o)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn list(&self, lp: &ListParams) -> Result<List<K>, Error> {
        decode(self.inner.list(lp).await?)
    }

    pub async fn create(&self, obj: &K) -> Result<K, Error> {
        decode(self.inner.create(&to_value::<K>(obj)?).await?)
    }

    pub async fn replace(&self, obj: &K) -> Result<K, Error> {
        decode(self.inner.replace(obj.metadata().name(), &to_value::<K>(obj)?).await?)
    }

    pub async fn replace_status(&self, obj: &K) -> Result<K, Error> {
        decode(self.inner.replace_status(obj.metadata().name(), &to_value::<K>(obj)?).await?)
    }

    pub async fn patch(&self, name: &str, pt: PatchType, patch: &Value) -> Result<K, Error> {
        decode(self.inner.patch(name, pt, patch).await?)
    }

    pub async fn patch_status(&self, name: &str, pt: PatchType, patch: &Value) -> Result<K, Error> {
        decode(self.inner.patch_status(name, pt, patch).await?)
    }

    pub async fn delete(&self, name: &str) -> Result<Value, Error> {
        self.inner.delete(name, None).await
    }

    pub async fn delete_with(&self, name: &str, dp: &DeleteParams) -> Result<Value, Error> {
        self.inner.delete_with(name, dp).await
    }

    pub async fn watch(&self, lp: &ListParams) -> Result<impl Stream<Item = Result<WatchEvent<K>, Error>> + use<K>, Error> {
        let s = self.inner.watch(lp).await?;
        Ok(s.map(|ev| {
            let ev = ev?;
            if ev.type_ == EventType::Bookmark {
                // Bookmarks carry only metadata; decode leniently.
                let obj: K = serde_json::from_value(ev.object.clone())
                    .or_else(|_| serde_json::from_value(json!({"metadata": ev.object["metadata"]})))
                    .map_err(|e| Error::Decode(e.to_string()))?;
                return Ok(WatchEvent { type_: ev.type_, object: obj });
            }
            Ok(WatchEvent { type_: ev.type_, object: decode(ev.object)? })
        }))
    }
}

fn to_value<K: Resource>(obj: &K) -> Result<Value, Error> {
    let mut v = serde_json::to_value(obj).map_err(|e| Error::Decode(e.to_string()))?;
    v["apiVersion"] = json!(K::api_version());
    v["kind"] = json!(K::KIND);
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_api::apps::v1::Deployment;
    use k8s_api::core::v1::{Node, Pod};

    #[test]
    fn paths() {
        assert_eq!(ApiResource::of::<Pod>().collection_path(Some("ns")), "/api/v1/namespaces/ns/pods");
        assert_eq!(ApiResource::of::<Pod>().collection_path(None), "/api/v1/pods");
        assert_eq!(ApiResource::of::<Node>().collection_path(Some("ignored")), "/api/v1/nodes");
        assert_eq!(ApiResource::of::<Deployment>().collection_path(Some("a")), "/apis/apps/v1/namespaces/a/deployments");
        let q = ListParams::default().labels("app in (a,b)").query(true);
        assert_eq!(q, "?labelSelector=app%20in%20%28a%2Cb%29&watch=true&allowWatchBookmarks=true");
    }
}
