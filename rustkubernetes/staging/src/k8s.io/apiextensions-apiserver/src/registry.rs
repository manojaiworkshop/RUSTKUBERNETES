//! Port of `apiextensions-apiserver/pkg/registry`:
//!
//! - [`CrdStrategy`] ports `customresourcedefinition/strategy.go` + `etcd.go`
//!   (defaults, `status.storedVersions`, the cleanup finalizer and the
//!   `Terminating` condition added on delete).
//! - [`CustomResourceStrategy`] ports `customresource/strategy.go`,
//!   `validator.go`, `tableconvertor`, plus the decode-time schema coercion
//!   of `customresource_handler.go` (metadata coercion, defaulting, pruning).

use std::sync::Arc;

use async_trait::async_trait;
use k8s_apimachinery::errors::ApiError;
use k8s_apimachinery::meta::human_age;
use k8s_apimachinery::unstructured::ObjectExt;
use k8s_apimachinery::validation::ErrorList;
use k8s_apiserver::registry::{ScalePaths, ScaleSelector, Strategy, StrategyContext};
use k8s_apiserver::table::Column;
use serde_json::{json, Value};

use crate::apis::{self, str_at};
use crate::jsonpath::{self, JsonPath};
use crate::{schema, validation};

// ---------------------------------------------------------------------------
// CustomResourceDefinition
// ---------------------------------------------------------------------------

pub struct CrdStrategy;

/// `status.storedVersions` gains the current storage version (`PrepareForCreate/Update`).
fn add_stored_version(crd: &mut Value) {
    let Some(sv) = apis::storage_version(crd) else { return };
    if !crd["status"].is_object() {
        crd["status"] = json!({});
    }
    let list = crd["status"].as_object_mut().unwrap().entry("storedVersions").or_insert_with(|| json!([]));
    if !list.is_array() {
        *list = json!([]);
    }
    let a = list.as_array_mut().unwrap();
    if !a.iter().any(|v| v == sv.as_str()) {
        a.push(json!(sv));
    }
}

#[async_trait]
impl Strategy for CrdStrategy {
    fn prepare_for_create(&self, obj: &mut Value) {
        apis::set_defaults(obj);
        obj["status"] = json!({"storedVersions": []});
        obj["status"]["storedVersions"] = json!([]);
        add_stored_version(obj);
    }

    fn prepare_for_update(&self, new: &mut Value, _old: &Value) {
        apis::set_defaults(new);
        add_stored_version(new);
    }

    fn validate(&self, obj: &Value) -> ErrorList {
        validation::validate(obj)
    }

    fn validate_update(&self, new: &Value, old: &Value) -> ErrorList {
        validation::validate_update(new, old)
    }

    fn validate_status_update(&self, new: &Value, _old: &Value) -> ErrorList {
        validation::validate_status(new)
    }

    /// A CRD is only removed once its cleanup finalizer ran (`etcd.go` `Delete`).
    fn can_remove(&self, obj: &Value) -> bool {
        obj.deletion_timestamp().is_some()
    }

    /// First delete request: add the cleanup finalizer and the `Terminating` condition.
    fn on_delete_marked(&self, obj: &mut Value) {
        let mut fins = obj.finalizers();
        if !fins.iter().any(|f| f == apis::CLEANUP_FINALIZER) {
            fins.push(apis::CLEANUP_FINALIZER.into());
            obj.metadata_mut().insert("finalizers".into(), json!(fins));
        }
        if !apis::is_condition_true(obj, apis::TERMINATING) {
            apis::set_condition(
                obj,
                apis::TERMINATING,
                "True",
                "InstanceDeletionPending",
                "CustomResourceDefinition marked for deletion; CustomResource deletion will begin soon",
            );
        }
    }

    /// `rest.NewDefaultTableConvertor`: `NAME`, `CREATED AT`.
    fn table_columns(&self) -> Vec<Column> {
        vec![Column::new("Created At", "date", "CreationTimestamp is a timestamp representing the server time when this object was created. It is not guaranteed to be set in happens-before order across separate operations. Clients may not set this value. It is represented in RFC3339 form and is in UTC.\n\nPopulated by the system. Read-only. Null for lists. More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#metadata")]
    }

    fn table_cells(&self, obj: &Value) -> Vec<Value> {
        vec![json!(obj.creation_timestamp())]
    }
}

// ---------------------------------------------------------------------------
// custom resources
// ---------------------------------------------------------------------------

/// One served version of a CRD.
pub struct CustomResourceStrategy {
    /// The CRD this strategy was built from.
    pub crd: Arc<Value>,
    pub version: String,
    columns: Vec<(Column, Option<JsonPath>)>,
}

impl CustomResourceStrategy {
    pub fn new(crd: Arc<Value>, version: &str) -> Self {
        let v = apis::version(&crd, version).cloned().unwrap_or(Value::Null);
        let cols = v.get("additionalPrinterColumns").and_then(Value::as_array).cloned().unwrap_or_default();
        let cols = if cols.is_empty() {
            // `serveDefaultColumnsIfEmpty`
            vec![json!({"name": "Age", "type": "date", "jsonPath": ".metadata.creationTimestamp",
                "description": "CreationTimestamp is a timestamp representing the server time when this object was created. It is not guaranteed to be set in happens-before order across separate operations. Clients may not set this value. It is represented in RFC3339 form and is in UTC.\n\nPopulated by the system. Read-only. Null for lists. More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#metadata"})]
        } else {
            cols
        };
        let columns = cols
            .iter()
            .map(|c| {
                let jp = str_at(c, "/jsonPath");
                let desc = match str_at(c, "/description") {
                    "" => format!("Custom resource definition column (in JSONPath format): {jp}"),
                    d => d.to_string(),
                };
                let col = Column {
                    name: str_at(c, "/name").to_string(),
                    type_: str_at(c, "/type").to_string(),
                    format: str_at(c, "/format").to_string(),
                    description: desc,
                    priority: c.get("priority").and_then(Value::as_i64).unwrap_or(0) as i32,
                };
                (col, JsonPath::parse(jp).ok())
            })
            .collect();
        Self { crd, version: version.to_string(), columns }
    }

    fn schema(&self) -> Option<&Value> {
        apis::schema(&self.crd, &self.version)
    }

    fn subresource(&self, name: &str) -> Option<&Value> {
        apis::version(&self.crd, &self.version)?.pointer(&format!("/subresources/{name}")).filter(|s| s.is_object())
    }

    /// The decode-time pipeline: metadata coercion, defaulting, pruning, null pruning.
    fn coerce(&self, obj: &mut Value) {
        schema::coerce_root_metadata(obj);
        if let Some(sc) = self.schema() {
            schema::default(obj, sc);
            schema::prune(obj, Some(sc), true);
            schema::prune_non_nullable_nulls(obj, sc);
        }
    }

    fn validate_object(&self, obj: &Value) -> ErrorList {
        let Some(sc) = self.schema() else { return vec![] };
        let mut errs = schema::validate(sc, obj);
        errs.extend(schema::validate_list_sets_and_maps(obj, sc));
        errs
    }

    fn terminating(&self) -> bool {
        apis::is_condition_true(&self.crd, apis::TERMINATING)
    }
}

/// `defaultDeprecationWarning`
fn default_deprecation_warning(crd: &Value, deprecated: &str) -> String {
    let (group, kind) = (str_at(crd, "/spec/group"), str_at(crd, "/spec/names/kind"));
    let msg = format!("{group}/{deprecated} {kind} is deprecated");
    let mut newer: Vec<String> = apis::versions(crd)
        .iter()
        .filter(|v| v["served"] == true && v["deprecated"] != true)
        .map(|v| str_at(v, "/name").to_string())
        .filter(|n| apis::compare_kube_aware_version(deprecated, n).is_lt())
        .collect();
    if newer.is_empty() {
        return msg;
    }
    newer.sort_by(|a, b| apis::compare_kube_aware_version(b, a));
    format!("{msg}; use {group}/{} {kind}", newer[0])
}

/// `schemaCoercingConverter`: after converting between versions, the object is
/// coerced to the target version's schema (metadata, pruning, nulls).
pub struct SchemaCoercingConverter {
    pub inner: Arc<dyn k8s_apiserver::storage::converting::Converter>,
    pub crd: Arc<Value>,
}

#[async_trait]
impl k8s_apiserver::storage::converting::Converter for SchemaCoercingConverter {
    async fn convert(&self, obj: Value, api_version: &str) -> Result<Value, String> {
        let mut out = self.inner.convert(obj, api_version).await?;
        let version = api_version.rsplit('/').next().unwrap_or("");
        schema::coerce_root_metadata(&mut out);
        if let Some(sc) = apis::schema(&self.crd, version) {
            schema::prune(&mut out, Some(sc), true);
            schema::prune_non_nullable_nulls(&mut out, sc);
        }
        Ok(out)
    }
}

/// `cellForJSONValue`
fn cell(ty: &str, v: &Value) -> Value {
    match (ty, v) {
        ("integer", Value::Number(n)) => n.as_i64().map(Value::from).or_else(|| n.as_f64().map(|f| json!(f as i64))).unwrap_or(Value::Null),
        ("number", Value::Number(n)) => n.as_f64().map(Value::from).unwrap_or(Value::Null),
        ("boolean", Value::Bool(_)) => v.clone(),
        ("date", Value::String(s)) => match chrono::DateTime::parse_from_rfc3339(s) {
            Ok(_) => json!(human_age(s)),
            Err(_) => json!("<invalid>"),
        },
        _ => Value::Null,
    }
}

#[async_trait]
impl Strategy for CustomResourceStrategy {
    fn has_status(&self) -> bool {
        self.subresource("status").is_some()
    }

    fn prepare_for_create(&self, obj: &mut Value) {
        self.coerce(obj);
    }

    fn prepare_for_update(&self, new: &mut Value, _old: &Value) {
        self.coerce(new);
    }

    fn prepare_for_subresource_update(&self, _sub: &str, new: &mut Value, _old: &Value) {
        self.coerce(new);
    }

    fn validate(&self, obj: &Value) -> ErrorList {
        self.validate_object(obj)
    }

    fn validate_status_update(&self, new: &Value, _old: &Value) -> ErrorList {
        self.validate_object(new)
    }

    fn unknown_fields(&self, obj: &Value) -> Vec<String> {
        let mut o = obj.clone();
        let mut unknown = schema::coerce_root_metadata(&mut o);
        if let Some(sc) = self.schema() {
            unknown.extend(schema::prune(&mut o, Some(sc), true));
        }
        unknown
    }

    async fn before_create(&self, _ctx: &StrategyContext<'_>, _obj: &mut Value) -> Result<(), ApiError> {
        if self.terminating() {
            return Err(ApiError::method_not_allowed("create not allowed while custom resource definition is terminating"));
        }
        Ok(())
    }

    fn scale_paths(&self) -> ScalePaths {
        let Some(sc) = self.subresource("scale") else { return ScalePaths::default() };
        let sel = str_at(sc, "/labelSelectorPath");
        ScalePaths {
            spec_replicas: str_at(sc, "/specReplicasPath").to_string(),
            status_replicas: str_at(sc, "/statusReplicasPath").to_string(),
            selector: ScaleSelector::Path((!sel.is_empty()).then(|| sel.to_string())),
        }
    }

    fn supports_strategic_merge_patch(&self) -> bool {
        false
    }

    /// `crdHandler`: requests for a deprecated version carry its deprecation warning.
    fn request_warning(&self) -> Option<String> {
        let v = apis::version(&self.crd, &self.version)?;
        if v["deprecated"] != true {
            return None;
        }
        match str_at(v, "/deprecationWarning") {
            "" => Some(default_deprecation_warning(&self.crd, &self.version)),
            w => Some(w.to_string()),
        }
    }

    /// `controller/openapi/builder.BuildOpenAPIV3`: the version's schema plus the
    /// implicit `apiVersion`, `kind` and `metadata`, tagged with its GVK.
    fn openapi_schema(&self) -> Option<(String, Value)> {
        let group = str_at(&self.crd, "/spec/group");
        let kind = str_at(&self.crd, "/status/acceptedNames/kind");
        let mut root = self.schema().cloned().unwrap_or_else(|| json!({"type": "object"}));
        let props = root.as_object_mut()?.entry("properties").or_insert_with(|| json!({}));
        props["apiVersion"] = json!({"type": "string", "description": "APIVersion defines the versioned schema of this representation of an object. Servers should convert recognized schemas to the latest internal value, and may reject unrecognized values. More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#resources"});
        props["kind"] = json!({"type": "string", "description": "Kind is a string value representing the REST resource this object represents. Servers may infer this from the endpoint the client submits requests to. Cannot be updated. In CamelCase. More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#types-kinds"});
        props["metadata"] = json!({"allOf": [{"$ref": format!("#/components/schemas/{}", k8s_apiserver::openapi::OBJECT_META)}],
            "description": "Standard object's metadata. More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#metadata"});
        root["x-kubernetes-group-version-kind"] = json!([{"group": group, "version": self.version, "kind": kind}]);
        let reversed: Vec<&str> = group.split('.').rev().collect();
        Some((format!("{}.{}.{kind}", reversed.join("."), self.version), root))
    }

    fn table_columns(&self) -> Vec<Column> {
        self.columns.iter().map(|(c, _)| c.clone()).collect()
    }

    fn table_cells(&self, obj: &Value) -> Vec<Value> {
        self.columns
            .iter()
            .map(|(c, jp)| {
                let Some(v) = jp.as_ref().and_then(|jp| jp.find(obj).into_iter().next()) else { return Value::Null };
                if c.type_ == "string" {
                    json!(jsonpath::text(v))
                } else {
                    cell(&c.type_, v)
                }
            })
            .collect()
    }
}
