//! Port of `k8s.io/apiserver/pkg/registry/rest` and `generic/registry`.
//!
//! A [`ResourceInfo`] describes one REST resource (`pods`, `deployments.apps`)
//! and carries its [`Strategy`]: the per-resource hooks for defaulting,
//! validation, field resetting, side effects (IP allocation, cascading) and
//! table printing. The generic handlers in [`crate::handlers`] implement the
//! CRUD logic once for every resource.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use k8s_apimachinery::errors::ApiError;
use k8s_apimachinery::validation::ErrorList;
use serde_json::Value;

use crate::storage::Storage;
use crate::table::{age_cell, Column};

/// Everything a strategy hook may need from the server.
pub struct StrategyContext<'a> {
    pub storage: &'a dyn Storage,
    pub registry: &'a Registry,
    /// For review resources (SubjectAccessReview, TokenReview).
    pub authorizer: &'a dyn crate::authorization::Authorizer,
    pub authenticator: Option<&'a crate::authentication::AuthenticatorChain>,
}

/// Per-resource behaviour (`rest.RESTCreateStrategy`, `RESTUpdateStrategy`,
/// `RESTDeleteStrategy`, `TableConvertor`, plus storage decorators).
#[async_trait]
pub trait Strategy: Send + Sync {
    /// Whether the object has a `status` that is only writable through `/status`.
    fn has_status(&self) -> bool {
        true
    }

    /// Defaulting and field resetting on create (`PrepareForCreate` + `SetDefaults_*`).
    /// The generic handler already removed `.status` if [`Strategy::has_status`].
    fn prepare_for_create(&self, _obj: &mut Value) {}

    /// Defaulting on update. The generic handler already copied `.status`
    /// from `old` (main endpoint) or `.spec` & co (status endpoint).
    fn prepare_for_update(&self, _new: &mut Value, _old: &Value) {}

    /// `Validate` (ObjectMeta is validated generically).
    fn validate(&self, _obj: &Value) -> ErrorList {
        vec![]
    }

    /// `ValidateUpdate`: defaults to [`Strategy::validate`] on the new object.
    fn validate_update(&self, new: &Value, _old: &Value) -> ErrorList {
        self.validate(new)
    }

    /// Validation for writes through the `/status` subresource.
    fn validate_status_update(&self, _new: &Value, _old: &Value) -> ErrorList {
        vec![]
    }

    /// Side effects before the object is persisted (e.g. ClusterIP allocation).
    async fn before_create(&self, _ctx: &StrategyContext<'_>, _obj: &mut Value) -> Result<(), ApiError> {
        Ok(())
    }

    /// Side effects before an update is persisted.
    async fn before_update(&self, _ctx: &StrategyContext<'_>, _new: &mut Value, _old: &Value) -> Result<(), ApiError> {
        Ok(())
    }

    /// Called when a create failed after `before_create` (to release allocations).
    async fn rollback_create(&self, _ctx: &StrategyContext<'_>, _obj: &Value) {}

    /// Called right before the object is removed from storage.
    async fn before_delete(&self, _ctx: &StrategyContext<'_>, _obj: &Value) -> Result<(), ApiError> {
        Ok(())
    }

    /// Called after the object was removed from storage.
    async fn after_delete(&self, _ctx: &StrategyContext<'_>, _obj: &Value) {}

    /// Whether the object may be removed from storage once it is marked for deletion
    /// and all `metadata.finalizers` are gone (namespaces also wait for `spec.finalizers`).
    fn can_remove(&self, _obj: &Value) -> bool {
        true
    }

    /// Called when a delete request only marks the object (finalizers pending),
    /// e.g. to set a namespace's phase to `Terminating`.
    fn on_delete_marked(&self, _obj: &mut Value) {}

    /// Graceful deletion: returns the grace period to use, or `None` to delete immediately.
    fn graceful_delete_seconds(&self, _obj: &Value, _requested: Option<i64>) -> Option<i64> {
        None
    }

    /// Called once at startup with all stored objects of this resource
    /// (used to rebuild in-memory allocators, like the Go repair controllers).
    async fn restore(&self, _existing: &[Value]) {}

    /// `ValidateNameFunc`: problems with `metadata.name` (default: DNS-1123 subdomain).
    fn validate_name(&self, name: &str) -> Vec<String> {
        k8s_apimachinery::validation::is_dns1123_subdomain(name)
    }

    /// Create-only resources that are computed per request and never stored
    /// (`SelfSubjectAccessReview`, `TokenReview`, ...).
    fn is_virtual(&self) -> bool {
        false
    }

    /// `POST` on a virtual resource: returns the object with its status filled in.
    async fn create_virtual(&self, _ctx: &StrategyContext<'_>, _namespace: &str, _obj: Value) -> Result<Value, ApiError> {
        Err(ApiError::method_not_allowed("create is not supported"))
    }

    /// POST to a custom subresource (`pods/binding`, `pods/eviction`).
    /// Returns `None` if the subresource is not supported.
    async fn create_subresource(
        &self,
        _ctx: &StrategyContext<'_>,
        _subresource: &str,
        _obj: &Value,
        _body: Value,
    ) -> Option<Result<Value, ApiError>> {
        None
    }

    /// `CONNECT`-style subresources (`pods/log`, `pods/exec`): returns the backend
    /// URL to proxy to (e.g. the kubelet), or `None` if unsupported.
    async fn connect(
        &self,
        _ctx: &StrategyContext<'_>,
        _subresource: &str,
        _obj: &Value,
        _query: &str,
    ) -> Option<Result<String, ApiError>> {
        None
    }

    /// Extra table columns after `NAME` (default: `AGE`).
    fn table_columns(&self) -> Vec<Column> {
        vec![Column::new("Age", "string", "")]
    }

    /// Cells matching [`Strategy::table_columns`].
    fn table_cells(&self, obj: &Value) -> Vec<Value> {
        vec![age_cell(obj)]
    }
}

/// A strategy with no special behaviour (used for simple resources).
pub struct DefaultStrategy {
    pub has_status: bool,
}

#[async_trait]
impl Strategy for DefaultStrategy {
    fn has_status(&self) -> bool {
        self.has_status
    }
}

/// Static description of one REST resource.
pub struct ResourceInfo {
    pub group: String,
    pub version: String,
    pub kind: String,
    /// plural, lowercase (`pods`)
    pub plural: String,
    /// singular, lowercase (`pod`)
    pub singular: String,
    pub short_names: Vec<String>,
    pub categories: Vec<String>,
    pub namespaced: bool,
    /// Supported subresources (`status`, `scale`, `binding`, ...).
    pub subresources: Vec<String>,
    pub strategy: Arc<dyn Strategy>,
}

impl ResourceInfo {
    /// `v1` or `apps/v1`
    pub fn api_version(&self) -> String {
        if self.group.is_empty() {
            self.version.clone()
        } else {
            format!("{}/{}", self.group, self.version)
        }
    }

    /// `pods` or `deployments.apps` (used in etcd keys and error messages).
    pub fn group_resource(&self) -> String {
        if self.group.is_empty() {
            self.plural.clone()
        } else {
            format!("{}.{}", self.plural, self.group)
        }
    }

    /// Storage key for one object.
    pub fn key(&self, namespace: &str, name: &str) -> String {
        if self.namespaced {
            format!("/registry/{}/{}/{}", self.group_resource(), namespace, name)
        } else {
            format!("/registry/{}/{}", self.group_resource(), name)
        }
    }

    /// Storage prefix for listing (all namespaces if `namespace` is empty).
    pub fn prefix(&self, namespace: &str) -> String {
        if self.namespaced && !namespace.is_empty() {
            format!("/registry/{}/{}/", self.group_resource(), namespace)
        } else {
            format!("/registry/{}/", self.group_resource())
        }
    }

    pub fn has_subresource(&self, sub: &str) -> bool {
        self.subresources.iter().any(|s| s == sub)
    }
}

/// Builder for [`ResourceInfo`].
pub struct ResourceBuilder {
    info: ResourceInfo,
}

impl ResourceBuilder {
    pub fn new(group: &str, version: &str, kind: &str, plural: &str, namespaced: bool) -> Self {
        Self {
            info: ResourceInfo {
                group: group.into(),
                version: version.into(),
                kind: kind.into(),
                plural: plural.into(),
                singular: kind.to_lowercase(),
                short_names: vec![],
                categories: vec![],
                namespaced,
                subresources: vec![],
                strategy: Arc::new(DefaultStrategy { has_status: false }),
            },
        }
    }

    pub fn short_names(mut self, names: &[&str]) -> Self {
        self.info.short_names = names.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn categories(mut self, cats: &[&str]) -> Self {
        self.info.categories = cats.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn subresources(mut self, subs: &[&str]) -> Self {
        self.info.subresources = subs.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn strategy(mut self, s: Arc<dyn Strategy>) -> Self {
        self.info.strategy = s;
        self
    }

    pub fn build(self) -> Arc<ResourceInfo> {
        Arc::new(self.info)
    }
}

/// All resources served by the API server.
#[derive(Default)]
pub struct Registry {
    resources: Vec<Arc<ResourceInfo>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, info: Arc<ResourceInfo>) {
        self.resources.push(info);
    }

    pub fn resources(&self) -> &[Arc<ResourceInfo>] {
        &self.resources
    }

    pub fn lookup(&self, group: &str, version: &str, plural: &str) -> Option<Arc<ResourceInfo>> {
        self.resources.iter().find(|r| r.group == group && r.version == version && r.plural == plural).cloned()
    }

    pub fn by_kind(&self, group: &str, kind: &str) -> Option<Arc<ResourceInfo>> {
        self.resources.iter().find(|r| r.group == group && r.kind == kind).cloned()
    }

    /// group → versions (in registration order).
    pub fn groups(&self) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for r in &self.resources {
            let v = out.entry(r.group.clone()).or_default();
            if !v.contains(&r.version) {
                v.push(r.version.clone());
            }
        }
        out
    }

    /// All resources of one group/version.
    pub fn group_version(&self, group: &str, version: &str) -> Vec<Arc<ResourceInfo>> {
        self.resources.iter().filter(|r| r.group == group && r.version == version).cloned().collect()
    }
}
