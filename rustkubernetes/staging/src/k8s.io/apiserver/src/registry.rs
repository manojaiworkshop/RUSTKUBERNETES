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

    /// `PrepareForUpdate` of a status-like subresource (`status`, `approval`),
    /// after the generic handler took `.status` from the request and the rest from `old`.
    fn prepare_for_subresource_update(&self, _sub: &str, _new: &mut Value, _old: &Value) {}

    /// Validation for writes through `/approval` (`certificatesigningrequests`).
    fn validate_approval_update(&self, _new: &Value, _old: &Value) -> ErrorList {
        vec![]
    }

    /// Whether `/status` writes may also change labels and annotations. Most
    /// status strategies call `ResetObjectMetaForStatus`; nodes and pods do not
    /// (the kubelet and network add-ons annotate nodes through `nodes/status`).
    fn status_updates_metadata(&self) -> bool {
        false
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

    /// Where the `scale` subresource reads and writes replicas.
    fn scale_paths(&self) -> ScalePaths {
        ScalePaths::default()
    }

    /// Whether `application/strategic-merge-patch+json` is accepted (custom
    /// resources have no patch strategy metadata, so they only take JSON and
    /// merge patches and apply).
    fn supports_strategic_merge_patch(&self) -> bool {
        true
    }

    /// Fields of an incoming object its schema does not declare (`spec.foo`),
    /// reported according to `?fieldValidation=` (Strict: error, Warn: warning).
    fn unknown_fields(&self, _obj: &Value) -> Vec<String> {
        vec![]
    }

    /// A warning returned with every request for this resource (deprecated
    /// custom resource versions).
    fn request_warning(&self) -> Option<String> {
        None
    }

    /// The kind's OpenAPI v3 schema as (component name, schema), published under
    /// `/openapi/v3` for `kubectl explain` (custom resources).
    fn openapi_schema(&self) -> Option<(String, Value)> {
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

/// JSON paths of the `scale` subresource (`.spec.replicas`, ...).
#[derive(Clone, Debug)]
pub struct ScalePaths {
    pub spec_replicas: String,
    pub status_replicas: String,
    pub selector: ScaleSelector,
}

#[derive(Clone, Debug)]
pub enum ScaleSelector {
    /// `.spec.selector.matchLabels` rendered as `k=v,...` (workload controllers).
    SpecMatchLabels,
    /// A string at a JSON path (`labelSelectorPath` of a custom resource); `None`: no selector.
    Path(Option<String>),
}

impl Default for ScalePaths {
    fn default() -> Self {
        Self {
            spec_replicas: ".spec.replicas".into(),
            status_replicas: ".status.replicas".into(),
            selector: ScaleSelector::SpecMatchLabels,
        }
    }
}

/// `.spec.replicas` → `/spec/replicas` (simple dotted JSON paths only).
pub fn json_path_pointer(path: &str) -> String {
    path.split('.').filter(|p| !p.is_empty()).map(|p| format!("/{}", p.replace('~', "~0").replace('/', "~1"))).collect()
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
    /// Storage key prefix when it is not derived from group and plural
    /// (custom resources: `<group>/<plural>`).
    pub storage_prefix: Option<String>,
    /// `apiVersion` objects are persisted with, when it differs from the served
    /// version (multi-version custom resources); reads and writes convert.
    pub storage_version: Option<String>,
    /// Converts between the served and the storage version.
    pub converter: Option<Arc<dyn crate::storage::converting::Converter>>,
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

    /// The resource's part of storage keys, as upstream lays out etcd
    /// (`StorageFactory.ResourcePrefix`): the plural without the group (with it
    /// for APIServices and CRDs), except nodes (`minions`), services (`services/specs`), endpoints
    /// (`services/endpoints`), replicationcontrollers (`controllers`) and
    /// ingresses (`ingress`).
    pub fn storage_prefix(&self) -> String {
        match &self.storage_prefix {
            Some(p) => p.clone(),
            None => etcd_resource_prefix(&self.group, &self.plural),
        }
    }

    /// Storage key for one object (`/registry/<prefix>/<namespace>/<name>`).
    pub fn key(&self, namespace: &str, name: &str) -> String {
        if self.namespaced {
            format!("/registry/{}/{}/{}", self.storage_prefix(), namespace, name)
        } else {
            format!("/registry/{}/{}", self.storage_prefix(), name)
        }
    }

    /// Storage prefix for listing (all namespaces if `namespace` is empty).
    pub fn prefix(&self, namespace: &str) -> String {
        if self.namespaced && !namespace.is_empty() {
            format!("/registry/{}/{}/", self.storage_prefix(), namespace)
        } else {
            format!("/registry/{}/", self.storage_prefix())
        }
    }

    pub fn has_subresource(&self, sub: &str) -> bool {
        self.subresources.iter().any(|s| s == sub)
    }

    /// The store as this resource's clients see it: objects in the served
    /// version even when they are persisted in another one.
    pub fn store(&self, storage: &Arc<dyn Storage>) -> Arc<dyn Storage> {
        match (&self.storage_version, &self.converter) {
            (Some(stored), Some(conv)) if *stored != self.api_version() => Arc::new(crate::storage::converting::ConvertingStorage {
                inner: storage.clone(),
                converter: conv.clone(),
                served: self.api_version(),
                stored: stored.clone(),
            }),
            _ => storage.clone(),
        }
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
                storage_prefix: None,
                storage_version: None,
                converter: None,
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

    pub fn singular(mut self, s: &str) -> Self {
        self.info.singular = s.to_string();
        self
    }

    pub fn storage_prefix(mut self, p: &str) -> Self {
        self.info.storage_prefix = Some(p.to_string());
        self
    }

    /// Objects are stored as `api_version` (see [`ResourceInfo::storage_version`]).
    pub fn storage_version(mut self, api_version: &str, converter: Arc<dyn crate::storage::converting::Converter>) -> Self {
        self.info.storage_version = Some(api_version.to_string());
        self.info.converter = Some(converter);
        self
    }

    pub fn build(self) -> Arc<ResourceInfo> {
        Arc::new(self.info)
    }
}

/// All resources served by the API server: the built-in ones registered at
/// start, plus resources added at runtime (`CustomResourceDefinition`s),
/// grouped by their owner so that one CRD's versions are swapped atomically.
#[derive(Default)]
pub struct Registry {
    resources: Vec<Arc<ResourceInfo>>,
    dynamic: std::sync::RwLock<BTreeMap<String, Vec<Arc<ResourceInfo>>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, info: Arc<ResourceInfo>) {
        self.resources.push(info);
    }

    /// Replaces every resource registered by `owner` (e.g. a CRD name) at runtime.
    /// An empty list removes the owner's resources.
    pub fn set_dynamic(&self, owner: &str, infos: Vec<Arc<ResourceInfo>>) {
        let mut d = self.dynamic.write().unwrap();
        if infos.is_empty() {
            d.remove(owner);
        } else {
            d.insert(owner.to_string(), infos);
        }
    }

    /// Resources `owner` registered at runtime.
    pub fn dynamic(&self, owner: &str) -> Vec<Arc<ResourceInfo>> {
        self.dynamic.read().unwrap().get(owner).cloned().unwrap_or_default()
    }

    /// Built-in resources followed by runtime-registered ones.
    pub fn resources(&self) -> Vec<Arc<ResourceInfo>> {
        let mut out = self.resources.clone();
        for v in self.dynamic.read().unwrap().values() {
            out.extend(v.iter().cloned());
        }
        out
    }

    pub fn lookup(&self, group: &str, version: &str, plural: &str) -> Option<Arc<ResourceInfo>> {
        let hit = |r: &&Arc<ResourceInfo>| r.group == group && r.version == version && r.plural == plural;
        if let Some(r) = self.resources.iter().find(hit) {
            return Some(r.clone());
        }
        self.dynamic.read().unwrap().values().flatten().find(hit).cloned()
    }

    pub fn by_kind(&self, group: &str, kind: &str) -> Option<Arc<ResourceInfo>> {
        self.resources().into_iter().find(|r| r.group == group && r.kind == kind)
    }

    /// group → versions (in registration order).
    pub fn groups(&self) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for r in self.resources() {
            let v = out.entry(r.group.clone()).or_default();
            if !v.contains(&r.version) {
                v.push(r.version.clone());
            }
        }
        out
    }

    /// All resources of one group/version.
    pub fn group_version(&self, group: &str, version: &str) -> Vec<Arc<ResourceInfo>> {
        self.resources().into_iter().filter(|r| r.group == group && r.version == version).collect()
    }
}

/// Upstream's etcd key prefix of a resource (see [`ResourceInfo::storage_prefix`]).
pub fn etcd_resource_prefix(group: &str, plural: &str) -> String {
    match (group, plural) {
        ("", "nodes") => "minions".into(),
        ("", "services") => "services/specs".into(),
        ("", "endpoints") => "services/endpoints".into(),
        ("", "replicationcontrollers") => "controllers".into(),
        ("networking.k8s.io", "ingresses") => "ingress".into(),
        // kube-aggregator keeps its group in the key.
        ("apiregistration.k8s.io", "apiservices") => "apiregistration.k8s.io/apiservices".into(),
        // so does apiextensions-apiserver.
        ("apiextensions.k8s.io", "customresourcedefinitions") => "apiextensions.k8s.io/customresourcedefinitions".into(),
        (_, p) => p.to_string(),
    }
}
