//! Port of `k8s.io/apiserver/pkg/endpoints/handlers` + `registry/generic/registry/store.go`.
//!
//! One generic implementation of get / list / watch / create / update /
//! patch / delete / deletecollection plus the `status` and `scale`
//! subresources, driven by each resource's [`Strategy`](crate::registry::Strategy).

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use base64::Engine;
use bytes::Bytes;
use futures::StreamExt;
use k8s_apimachinery::errors::ApiError;
use k8s_apimachinery::fields::FieldSelector;
use k8s_apimachinery::labels::Selector;
use k8s_apimachinery::meta::{new_uid, now_rfc3339, random_suffix};
use k8s_apimachinery::unstructured::ObjectExt;
use k8s_apimachinery::validation::{self as v, FieldError};
use k8s_apimachinery::watch::EventType;
use serde_json::{json, Value};

use crate::admission::{Attributes, Operation};
use crate::patch::{apply_patch, PatchType};
use crate::registry::ResourceInfo;
use crate::request::{Query, ResourceRequest};
use crate::server::ApiServer;
use crate::storage::{parse_rv, StorageError, StoredEvent};
use crate::table::{build_table, wants_table, IncludeObject};

pub type HandlerResult = Result<Response, ApiError>;

/// Finalizer added by `propagationPolicy=Foreground`.
pub const FOREGROUND_FINALIZER: &str = "foregroundDeletion";
/// Finalizer added by `propagationPolicy=Orphan`.
pub const ORPHAN_FINALIZER: &str = "orphan";

// ---------------------------------------------------------------------------
// response helpers
// ---------------------------------------------------------------------------

pub fn json_response(code: u16, body: &Value) -> Response {
    Response::builder()
        .status(StatusCode::from_u16(code).unwrap_or(StatusCode::OK))
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap_or_default()))
        .unwrap()
}

pub fn error_response(err: &ApiError) -> Response {
    json_response(err.code, &serde_json::to_value(err.to_status()).unwrap())
}

fn storage_err(info: &ResourceInfo, name: &str, e: StorageError) -> ApiError {
    match e {
        StorageError::NotFound(_) => ApiError::not_found(&info.group, &info.plural, name),
        StorageError::AlreadyExists(_) => ApiError::already_exists(&info.group, &info.plural, name),
        StorageError::Conflict(_) => ApiError::conflict_modified(&info.group, &info.plural, name),
        StorageError::Gone { requested, oldest } => ApiError::gone(format!("too old resource version: {requested} ({oldest})")),
        StorageError::Internal(m) => ApiError::internal(m),
    }
}

fn decode_body(s: &ApiServer, headers: &HeaderMap, body: &[u8]) -> Result<Value, ApiError> {
    if body.is_empty() {
        return Err(ApiError::bad_request("the request body is empty"));
    }
    let ct = headers.get("content-type").and_then(|h| h.to_str().ok()).unwrap_or("application/json");
    let media = ct.split(';').next().unwrap_or("").trim();
    let v: Value = if media == "application/vnd.kubernetes.protobuf" {
        let dec = s.protobuf_decoder.ok_or_else(|| ApiError::unsupported_media_type(ct))?;
        dec(body).map_err(|e| ApiError::bad_request(format!("error decoding protobuf: {e}")))?
    } else if media == "application/cbor" {
        return Err(ApiError::unsupported_media_type(ct));
    } else if ct.contains("yaml") {
        serde_yaml::from_slice(body).map_err(|e| ApiError::bad_request(format!("error decoding YAML: {e}")))?
    } else {
        serde_json::from_slice(body).map_err(|e| ApiError::bad_request(format!("error decoding JSON: {e}")))?
    };
    if !v.is_object() {
        return Err(ApiError::bad_request("the request body must be a JSON object"));
    }
    Ok(v)
}

fn accept(headers: &HeaderMap) -> String {
    headers.get("accept").and_then(|h| h.to_str().ok()).unwrap_or("").to_string()
}

fn dry_run(q: &Query) -> bool {
    q.get("dryRun") == Some("All")
}

/// `?fieldValidation=`: what to do about fields the schema does not declare.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldValidation {
    Ignore,
    /// The default: each unknown field becomes a `Warning` header.
    Warn,
    Strict,
}

impl FieldValidation {
    fn from_query(q: &Query) -> Result<Self, ApiError> {
        match q.get("fieldValidation").unwrap_or("") {
            "" | "Warn" => Ok(Self::Warn),
            "Ignore" => Ok(Self::Ignore),
            "Strict" => Ok(Self::Strict),
            other => Err(ApiError::bad_request(format!(
                "fieldValidation parameter unsupported: {other:?}. Supported values: Ignore, Warn, Strict"
            ))),
        }
    }

    /// Reports `unknown` fields (`schemaCoercingDecoder` + `addStrictDecodingWarnings`).
    fn check(self, unknown: Vec<String>) -> Result<(), ApiError> {
        if unknown.is_empty() {
            return Ok(());
        }
        let errs: Vec<String> = unknown.iter().map(|f| format!("unknown field \"{f}\"")).collect();
        match self {
            Self::Ignore => Ok(()),
            Self::Warn => {
                errs.into_iter().for_each(crate::warning::add);
                Ok(())
            }
            Self::Strict => Err(ApiError::bad_request(format!("strict decoding error: {}", errs.join(", ")))),
        }
    }
}

/// Everything except metadata/status: if this changes, `generation` is bumped.
fn spec_part(obj: &Value) -> Value {
    let mut o = obj.clone();
    if let Some(m) = o.as_object_mut() {
        m.remove("metadata");
        m.remove("status");
        m.remove("apiVersion");
        m.remove("kind");
    }
    o
}

/// Selectors from `?labelSelector=` / `?fieldSelector=`.
struct Filter {
    labels: Selector,
    fields: FieldSelector,
}

impl Filter {
    fn from_query(q: &Query) -> Result<Self, ApiError> {
        let labels = Selector::parse(q.get("labelSelector").unwrap_or("")).map_err(ApiError::bad_request)?;
        let fields = FieldSelector::parse(q.get("fieldSelector").unwrap_or("")).map_err(ApiError::bad_request)?;
        Ok(Self { labels, fields })
    }

    fn matches(&self, obj: &Value) -> bool {
        self.labels.matches(&obj.labels()) && self.fields.matches(obj)
    }
}

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

pub async fn handle_resource(
    s: &Arc<ApiServer>,
    method: &Method,
    req: ResourceRequest,
    q: &Query,
    headers: &HeaderMap,
    body: Bytes,
) -> HandlerResult {
    let info = s
        .registry
        .lookup(&req.group, &req.version, &req.resource)
        .ok_or_else(|| ApiError::new(404, "NotFound", "the server could not find the requested resource"))?;
    if !info.namespaced && !req.namespace.is_empty() {
        return Err(ApiError::new(404, "NotFound", "the server could not find the requested resource"));
    }
    if let Some(w) = info.strategy.request_warning() {
        crate::warning::add(w);
    }
    let ns = req.namespace.as_str();
    if info.strategy.is_virtual() {
        return match (&req.name, method.clone()) {
            (None, Method::POST) => {
                let obj = decode_body(s, headers, &body)?;
                check_type(&info, &mut obj.clone())?;
                let out = info.strategy.create_virtual(&s.ctx(), ns, obj).await?;
                Ok(json_response(201, &out))
            }
            _ => Err(ApiError::method_not_allowed(format!("{method} is not supported on {}", info.plural))),
        };
    }
    match (&req.name, &req.subresource) {
        (None, _) => match *method {
            Method::GET if q.flag("watch") => watch(s, &info, ns, q, headers).await,
            Method::GET => list(s, &info, ns, q, headers).await,
            Method::POST => create(s, &info, ns, q, headers, body).await,
            Method::DELETE => delete_collection(s, &info, ns, q, headers, body).await,
            _ => Err(ApiError::method_not_allowed(format!("{method} is not supported on a collection"))),
        },
        (Some(name), None) => match *method {
            Method::GET if q.flag("watch") => {
                let mut q2 = q.clone();
                q2.0.insert("fieldSelector".into(), format!("metadata.name={name}"));
                watch(s, &info, ns, &q2, headers).await
            }
            Method::GET => get(s, &info, ns, name, q, headers).await,
            Method::PUT => update(s, &info, ns, name, q, headers, body).await,
            Method::PATCH => patch(s, &info, ns, name, None, q, headers, body).await,
            Method::DELETE => delete(s, &info, ns, name, q, headers, body).await,
            _ => Err(ApiError::method_not_allowed(format!("{method} is not supported"))),
        },
        (Some(name), Some(sub)) => {
            if !info.has_subresource(sub) {
                return Err(ApiError::new(404, "NotFound", "the server could not find the requested resource"));
            }
            match (sub.as_str(), method.clone()) {
                ("status" | "approval", Method::GET) => get(s, &info, ns, name, q, headers).await,
                ("status" | "approval", Method::PUT) => update_status(s, &info, ns, name, sub, q, headers, body).await,
                ("status" | "approval", Method::PATCH) => patch(s, &info, ns, name, Some(sub), q, headers, body).await,
                ("finalize", Method::PUT) => update_finalize(s, &info, ns, name, q, headers, body).await,
                ("log", Method::GET) => {
                    let url = connect_url(s, &info, ns, name, "log", q).await?;
                    crate::proxy::proxy_http(&url, s.kubelet_tls.clone()).await
                }
                ("scale", Method::GET) => get_scale(s, &info, ns, name).await,
                ("scale", Method::PUT) => update_scale(s, &info, ns, name, q, headers, body).await,
                ("scale", Method::PATCH) => patch_scale(s, &info, ns, name, q, headers, body).await,
                (sub, Method::POST) => create_subresource(s, &info, ns, name, sub, headers, body).await,
                _ => Err(ApiError::method_not_allowed(format!("{method} is not supported on {sub}"))),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// read
// ---------------------------------------------------------------------------

async fn get(s: &Arc<ApiServer>, info: &Arc<ResourceInfo>, ns: &str, name: &str, q: &Query, headers: &HeaderMap) -> HandlerResult {
    let obj = info.store(&s.storage).get(&info.key(ns, name)).await.map_err(|e| storage_err(info, name, e))?;
    if wants_table(&accept(headers)) {
        let rv = obj.resource_version().to_string();
        let t = build_table(
            &info.strategy.table_columns(),
            &[obj],
            |o| info.strategy.table_cells(o),
            IncludeObject::parse(q.get("includeObject")),
            &rv,
            None,
            true,
        );
        return Ok(json_response(200, &t));
    }
    Ok(json_response(200, &obj))
}

async fn list(s: &Arc<ApiServer>, info: &Arc<ResourceInfo>, ns: &str, q: &Query, headers: &HeaderMap) -> HandlerResult {
    let filter = Filter::from_query(q)?;
    let (items, rev) = info.store(&s.storage).list(&info.prefix(ns)).await.map_err(|e| storage_err(info, "", e))?;
    let mut items: Vec<Value> = items.into_iter().filter(|o| filter.matches(o)).collect();

    // Pagination: the continue token is the storage key of the last returned item.
    if let Some(token) = q.get("continue") {
        let last_key = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(token)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .ok_or_else(|| ApiError::bad_request("continue key is not valid"))?;
        items.retain(|o| info.key(o.namespace(), o.name()) > last_key);
    }
    let mut continue_token = None;
    let mut remaining = None;
    if let Some(limit) = q.int("limit").filter(|l| *l > 0) {
        let limit = limit as usize;
        if items.len() > limit {
            remaining = Some(items.len() - limit);
            items.truncate(limit);
            let last = items.last().unwrap();
            let key = info.key(last.namespace(), last.name());
            continue_token = Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key));
        }
    }

    let rv = rev.to_string();
    if wants_table(&accept(headers)) {
        let t = build_table(
            &info.strategy.table_columns(),
            &items,
            |o| info.strategy.table_cells(o),
            IncludeObject::parse(q.get("includeObject")),
            &rv,
            continue_token.as_deref(),
            true,
        );
        return Ok(json_response(200, &t));
    }
    let mut meta = json!({"resourceVersion": rv});
    if let Some(c) = continue_token {
        meta["continue"] = json!(c);
        meta["remainingItemCount"] = json!(remaining.unwrap_or(0));
    }
    Ok(json_response(
        200,
        &json!({
            "kind": format!("{}List", info.kind),
            "apiVersion": info.api_version(),
            "metadata": meta,
            "items": items,
        }),
    ))
}

// ---------------------------------------------------------------------------
// watch
// ---------------------------------------------------------------------------

/// Maps a storage event through the selector (objects moving in/out of the
/// selector become ADDED/DELETED, like the Go watch cache).
fn translate(ev: &StoredEvent, filter: &Filter) -> Option<(EventType, Value)> {
    let cur = filter.matches(&ev.object);
    let prev = ev.prev.as_ref().map(|p| filter.matches(p)).unwrap_or(false);
    match ev.type_ {
        EventType::Added => cur.then(|| (EventType::Added, ev.object.clone())),
        EventType::Deleted => (prev || cur).then(|| (EventType::Deleted, ev.object.clone())),
        EventType::Modified => match (prev, cur) {
            (true, true) => Some((EventType::Modified, ev.object.clone())),
            (false, true) => Some((EventType::Added, ev.object.clone())),
            (true, false) => Some((EventType::Deleted, ev.object.clone())),
            (false, false) => None,
        },
        _ => None,
    }
}

async fn watch(s: &Arc<ApiServer>, info: &Arc<ResourceInfo>, ns: &str, q: &Query, headers: &HeaderMap) -> HandlerResult {
    let filter = Filter::from_query(q)?;
    let prefix = info.prefix(ns);
    let rv_param = q.get("resourceVersion").unwrap_or("");
    let send_initial = q.flag("sendInitialEvents");
    let bookmarks = q.flag("allowWatchBookmarks");
    let timeout = Duration::from_secs(q.int("timeoutSeconds").filter(|t| *t > 0).unwrap_or(1800) as u64);
    let table = wants_table(&accept(headers));
    let include = IncludeObject::parse(q.get("includeObject"));

    // "" / "0" (or sendInitialEvents): synthetic ADDED events for the current state first.
    let (initial, start) = if rv_param.is_empty() || rv_param == "0" || send_initial {
        let (items, rev) = info.store(&s.storage).list(&prefix).await.map_err(|e| storage_err(info, "", e))?;
        (items.into_iter().filter(|o| filter.matches(o)).collect::<Vec<_>>(), rev)
    } else {
        (vec![], parse_rv(rv_param).map_err(ApiError::bad_request)?.unwrap_or(0))
    };

    let storage_stream = info.store(&s.storage).watch(&prefix, start).await;
    let info = info.clone();
    let kind = info.kind.clone();
    let api_version = info.api_version();

    let stream = async_stream::stream! {
        let mut first_table = true;
        let mut encode = |type_: EventType, obj: Value| -> Bytes {
            let object = if table && type_ != EventType::Bookmark && type_ != EventType::Error {
                let t = build_table(
                    &info.strategy.table_columns(), std::slice::from_ref(&obj),
                    |o| info.strategy.table_cells(o), include, obj.resource_version(), None, first_table,
                );
                first_table = false;
                t
            } else {
                obj
            };
            let mut line = serde_json::to_vec(&json!({"type": type_.as_str(), "object": object})).unwrap_or_default();
            line.push(b'\n');
            Bytes::from(line)
        };

        let mut stream = match storage_stream {
            Ok(st) => st,
            Err(e) => {
                let err = storage_err(&info, "", e);
                yield encode(EventType::Error, serde_json::to_value(err.to_status()).unwrap());
                return;
            }
        };
        for obj in initial {
            yield encode(EventType::Added, obj);
        }
        let mut last_rv = start;
        if send_initial {
            yield encode(EventType::Bookmark, json!({
                "kind": kind, "apiVersion": api_version,
                "metadata": {"resourceVersion": start.to_string(),
                             "annotations": {"k8s.io/initial-events-end": "true"}}}));
        }
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        let mut bookmark_tick = tokio::time::interval(Duration::from_secs(60));
        bookmark_tick.tick().await;
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                _ = bookmark_tick.tick(), if bookmarks => {
                    yield encode(EventType::Bookmark, json!({
                        "kind": kind, "apiVersion": api_version,
                        "metadata": {"resourceVersion": last_rv.to_string()}}));
                }
                ev = stream.next() => match ev {
                    None => break,
                    Some(Err(e)) => {
                        let err = storage_err(&info, "", e);
                        yield encode(EventType::Error, serde_json::to_value(err.to_status()).unwrap());
                        break;
                    }
                    Some(Ok(ev)) => {
                        last_rv = ev.revision;
                        if let Some((t, obj)) = translate(&ev, &filter) {
                            yield encode(t, obj);
                        }
                    }
                }
            }
        }
    };
    let body = Body::from_stream(stream.map(Ok::<Bytes, Infallible>));
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "application/json")
        .header("Transfer-Encoding", "chunked")
        .body(body)
        .unwrap())
}

// ---------------------------------------------------------------------------
// create
// ---------------------------------------------------------------------------

/// Generic `ObjectMeta` validation (`ValidateObjectMeta`).
pub fn validate_object_meta(info: &ResourceInfo, obj: &Value) -> Vec<FieldError> {
    let mut errs = vec![];
    let name = obj.name();
    if name.is_empty() {
        errs.push(FieldError::required("metadata.name", "name or generateName is required"));
    } else {
        for m in info.strategy.validate_name(name) {
            errs.push(FieldError::invalid("metadata.name", name, m));
        }
    }
    if info.namespaced {
        if obj.namespace().is_empty() {
            errs.push(FieldError::required("metadata.namespace", ""));
        }
    } else if !obj.namespace().is_empty() {
        errs.push(FieldError::forbidden("metadata.namespace", "not allowed on this type"));
    }
    if let Some(l) = obj.pointer("/metadata/labels") {
        match serde_json::from_value(l.clone()) {
            Ok(m) => errs.extend(v::validate_labels(&m, "metadata.labels")),
            Err(_) => errs.push(FieldError::invalid("metadata.labels", l, "labels must be a map of strings")),
        }
    }
    if let Some(a) = obj.pointer("/metadata/annotations") {
        match serde_json::from_value(a.clone()) {
            Ok(m) => errs.extend(v::validate_annotations(&m, "metadata.annotations")),
            Err(_) => errs.push(FieldError::invalid("metadata.annotations", a, "annotations must be a map of strings")),
        }
    }
    errs
}

fn check_type(info: &ResourceInfo, obj: &mut Value) -> Result<(), ApiError> {
    let kind = obj.kind().to_string();
    if !kind.is_empty() && kind != info.kind {
        return Err(ApiError::bad_request(format!("the kind of the provided object ({kind}) does not match the resource ({})", info.kind)));
    }
    let av = obj.api_version().to_string();
    if !av.is_empty() && av != info.api_version() {
        return Err(ApiError::bad_request(format!(
            "the API version in the data ({av}) does not match the expected API version ({})",
            info.api_version()
        )));
    }
    obj["kind"] = json!(info.kind);
    obj["apiVersion"] = json!(info.api_version());
    Ok(())
}

pub(crate) async fn create_object(
    s: &Arc<ApiServer>,
    info: &Arc<ResourceInfo>,
    ns: &str,
    mut obj: Value,
    dry: bool,
    fv: FieldValidation,
) -> Result<Value, ApiError> {
    check_type(info, &mut obj)?;
    fv.check(info.strategy.unknown_fields(&obj))?;
    // namespace
    if info.namespaced {
        let body_ns = obj.namespace().to_string();
        if !body_ns.is_empty() && !ns.is_empty() && body_ns != ns {
            return Err(ApiError::bad_request("the namespace of the provided object does not match the namespace sent on the request"));
        }
        let effective = if ns.is_empty() { body_ns } else { ns.to_string() };
        obj.metadata_mut().insert("namespace".into(), json!(effective));
    } else {
        // `BeforeCreate`: a namespace on a cluster-scoped object is dropped.
        obj.metadata_mut().remove("namespace");
    }
    // generateName
    if obj.name().is_empty() {
        if let Some(prefix) = obj.pointer("/metadata/generateName").and_then(Value::as_str).map(String::from) {
            if !prefix.is_empty() {
                obj.metadata_mut().insert("name".into(), json!(format!("{prefix}{}", random_suffix(5))));
            }
        }
    }
    // system-managed metadata
    {
        let m = obj.metadata_mut();
        m.insert("uid".into(), json!(new_uid()));
        m.insert("creationTimestamp".into(), json!(now_rfc3339()));
        m.insert("generation".into(), json!(1));
        m.remove("resourceVersion");
        m.remove("deletionTimestamp");
        m.remove("deletionGracePeriodSeconds");
        m.remove("selfLink");
        m.remove("managedFields");
    }
    if info.strategy.has_status() {
        if let Some(o) = obj.as_object_mut() {
            o.remove("status");
        }
    }
    info.strategy.prepare_for_create(&mut obj);

    let ctx = s.ctx();
    let name = obj.name().to_string();
    let namespace = obj.namespace().to_string();
    let attrs = Attributes {
        operation: Operation::Create,
        resource: info,
        subresource: None,
        namespace: &namespace,
        name: &name,
        old_object: None,
        dry_run: dry,
    };
    s.admission.admit(&attrs, &mut obj, &ctx).await?;

    let mut errs = validate_object_meta(info, &obj);
    errs.extend(info.strategy.validate(&obj));
    if !errs.is_empty() {
        return Err(ApiError::invalid(&info.group, &info.kind, &name, &errs));
    }
    s.admission.validate(&attrs, &obj, &ctx).await?;
    if dry {
        return Ok(obj);
    }
    info.strategy.before_create(&ctx, &mut obj).await?;
    match info.store(&s.storage).create(&info.key(&namespace, &name), obj.clone()).await {
        Ok(created) => Ok(created),
        Err(e) => {
            info.strategy.rollback_create(&ctx, &obj).await;
            Err(storage_err(info, &name, e))
        }
    }
}

async fn create(s: &Arc<ApiServer>, info: &Arc<ResourceInfo>, ns: &str, q: &Query, headers: &HeaderMap, body: Bytes) -> HandlerResult {
    let obj = decode_body(s, headers, &body)?;
    let created = create_object(s, info, ns, obj, dry_run(q), FieldValidation::from_query(q)?).await?;
    Ok(json_response(201, &created))
}

// ---------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum UpdateMode {
    Main,
    Status,
    /// `namespaces/<name>/finalize`: only `spec.finalizers` may change.
    Finalize,
    /// `certificatesigningrequests/<name>/approval`: like `status`, the strategy
    /// keeps only the conditions.
    Approval,
}

/// Runs the update pipeline for `new` replacing `old` and persists it.
#[allow(clippy::too_many_arguments)]
async fn update_object(
    s: &Arc<ApiServer>,
    info: &Arc<ResourceInfo>,
    old: &Value,
    mut new: Value,
    expected: Option<u64>,
    mode: UpdateMode,
    dry: bool,
    fv: FieldValidation,
) -> Result<Value, ApiError> {
    let name = old.name().to_string();
    let namespace = old.namespace().to_string();
    check_type(info, &mut new)?;
    if mode != UpdateMode::Finalize {
        fv.check(info.strategy.unknown_fields(&new))?;
    }
    if !new.name().is_empty() && new.name() != name {
        return Err(ApiError::bad_request(format!("the name of the object ({}) does not match the name on the URL ({name})", new.name())));
    }

    if mode == UpdateMode::Finalize {
        let fins = new.pointer("/spec/finalizers").cloned().unwrap_or(json!([]));
        let rv = new.resource_version().to_string();
        new = old.clone();
        new["spec"]["finalizers"] = fins;
        new.metadata_mut().insert("resourceVersion".into(), json!(rv));
    } else if matches!(mode, UpdateMode::Status | UpdateMode::Approval) {
        // Only .status may change through /status (ResetObjectMetaForStatus).
        let status = new.get("status").cloned();
        let rv = new.resource_version().to_string();
        let meta = info.strategy.status_updates_metadata().then(|| {
            let m = new.get("metadata");
            (m.and_then(|m| m.get("labels")).cloned(), m.and_then(|m| m.get("annotations")).cloned())
        });
        new = old.clone();
        if let Some((labels, annotations)) = meta {
            for (k, v) in [("labels", labels), ("annotations", annotations)] {
                match v {
                    Some(v) => {
                        new.metadata_mut().insert(k.into(), v);
                    }
                    None => {
                        new.metadata_mut().remove(k);
                    }
                }
            }
        }
        match status {
            Some(st) => new["status"] = st,
            None => {
                if let Some(o) = new.as_object_mut() {
                    o.remove("status");
                }
            }
        }
        new.metadata_mut().insert("resourceVersion".into(), json!(rv));
    } else if info.strategy.has_status() {
        match old.get("status") {
            Some(st) => new["status"] = st.clone(),
            None => {
                if let Some(o) = new.as_object_mut() {
                    o.remove("status");
                }
            }
        }
    }

    // Immutable / system-managed metadata comes from the stored object.
    {
        let old_meta = old.get("metadata").cloned().unwrap_or(json!({}));
        let m = new.metadata_mut();
        m.insert("name".into(), json!(name));
        if info.namespaced {
            m.insert("namespace".into(), json!(namespace));
        }
        for k in ["uid", "creationTimestamp", "deletionTimestamp", "deletionGracePeriodSeconds", "generation"] {
            match old_meta.get(k) {
                Some(v) => m.insert(k.into(), v.clone()),
                None => m.remove(k),
            };
        }
        m.remove("managedFields");
    }
    if mode == UpdateMode::Main && spec_part(&new) != spec_part(old) {
        let g = old.pointer("/metadata/generation").and_then(Value::as_i64).unwrap_or(0) + 1;
        new.metadata_mut().insert("generation".into(), json!(g));
    }
    match mode {
        UpdateMode::Main => info.strategy.prepare_for_update(&mut new, old),
        UpdateMode::Status => info.strategy.prepare_for_subresource_update("status", &mut new, old),
        UpdateMode::Approval => info.strategy.prepare_for_subresource_update("approval", &mut new, old),
        UpdateMode::Finalize => {}
    }

    let ctx = s.ctx();
    let attrs = Attributes {
        operation: Operation::Update,
        resource: info,
        subresource: match mode {
            UpdateMode::Main => None,
            UpdateMode::Status => Some("status"),
            UpdateMode::Finalize => Some("finalize"),
            UpdateMode::Approval => Some("approval"),
        },
        namespace: &namespace,
        name: &name,
        old_object: Some(old),
        dry_run: dry,
    };
    s.admission.admit(&attrs, &mut new, &ctx).await?;
    let mut errs = validate_object_meta(info, &new);
    errs.extend(match mode {
        UpdateMode::Main => info.strategy.validate_update(&new, old),
        UpdateMode::Status => info.strategy.validate_status_update(&new, old),
        UpdateMode::Approval => info.strategy.validate_approval_update(&new, old),
        UpdateMode::Finalize => vec![],
    });
    // A deleting object may not gain finalizers back once they are all gone; a
    // deletionTimestamp can never be removed (it was copied from old above).
    if !errs.is_empty() {
        return Err(ApiError::invalid(&info.group, &info.kind, &name, &errs));
    }
    s.admission.validate(&attrs, &new, &ctx).await?;

    // No-op update: do not write (keeps resourceVersion stable, no watch event).
    let mut cmp_new = new.clone();
    let mut cmp_old = old.clone();
    cmp_new.metadata_mut().remove("resourceVersion");
    cmp_old.metadata_mut().remove("resourceVersion");
    if cmp_new == cmp_old {
        if let Some(exp) = expected {
            if old.resource_version() != exp.to_string() {
                return Err(ApiError::conflict_modified(&info.group, &info.plural, &name));
            }
        }
        return Ok(old.clone());
    }
    if dry {
        return Ok(new);
    }
    info.strategy.before_update(&ctx, &mut new, old).await?;
    let key = info.key(&namespace, &name);
    let updated = info.store(&s.storage).update(&key, expected, new).await.map_err(|e| storage_err(info, &name, e))?;
    finalize_if_ready(s, info, updated).await
}

/// When the last finalizer is removed from an object marked for deletion, delete it.
async fn finalize_if_ready(s: &Arc<ApiServer>, info: &Arc<ResourceInfo>, obj: Value) -> Result<Value, ApiError> {
    let grace = obj.pointer("/metadata/deletionGracePeriodSeconds").and_then(Value::as_i64).unwrap_or(0);
    if obj.deletion_timestamp().is_some() && obj.finalizers().is_empty() && grace == 0 && info.strategy.can_remove(&obj) {
        return remove_object(s, info, &obj).await;
    }
    Ok(obj)
}

async fn update(
    s: &Arc<ApiServer>,
    info: &Arc<ResourceInfo>,
    ns: &str,
    name: &str,
    q: &Query,
    headers: &HeaderMap,
    body: Bytes,
) -> HandlerResult {
    let new = decode_body(s, headers, &body)?;
    let old = info.store(&s.storage).get(&info.key(ns, name)).await.map_err(|e| storage_err(info, name, e))?;
    let expected = parse_rv(new.resource_version()).map_err(ApiError::bad_request)?;
    let out = update_object(s, info, &old, new, expected, UpdateMode::Main, dry_run(q), FieldValidation::from_query(q)?).await?;
    Ok(json_response(200, &out))
}

#[allow(clippy::too_many_arguments)]
async fn update_status(
    s: &Arc<ApiServer>,
    info: &Arc<ResourceInfo>,
    ns: &str,
    name: &str,
    sub: &str,
    q: &Query,
    headers: &HeaderMap,
    body: Bytes,
) -> HandlerResult {
    let new = decode_body(s, headers, &body)?;
    let old = info.store(&s.storage).get(&info.key(ns, name)).await.map_err(|e| storage_err(info, name, e))?;
    let expected = parse_rv(new.resource_version()).map_err(ApiError::bad_request)?;
    let mode = if sub == "approval" { UpdateMode::Approval } else { UpdateMode::Status };
    let out = update_object(s, info, &old, new, expected, mode, dry_run(q), FieldValidation::from_query(q)?).await?;
    Ok(json_response(200, &out))
}

async fn update_finalize(
    s: &Arc<ApiServer>,
    info: &Arc<ResourceInfo>,
    ns: &str,
    name: &str,
    q: &Query,
    headers: &HeaderMap,
    body: Bytes,
) -> HandlerResult {
    let new = decode_body(s, headers, &body)?;
    let old = info.store(&s.storage).get(&info.key(ns, name)).await.map_err(|e| storage_err(info, name, e))?;
    let expected = parse_rv(new.resource_version()).map_err(ApiError::bad_request)?;
    let out = update_object(s, info, &old, new, expected, UpdateMode::Finalize, dry_run(q), FieldValidation::Ignore).await?;
    Ok(json_response(200, &out))
}

// ---------------------------------------------------------------------------
// patch
// ---------------------------------------------------------------------------

/// The patch type of a `PATCH` request, if the resource accepts it.
fn patch_type(info: &ResourceInfo, ct: &str) -> Result<PatchType, ApiError> {
    let pt = PatchType::from_content_type(ct);
    if !info.strategy.supports_strategic_merge_patch() && !matches!(pt, Some(PatchType::Json | PatchType::Merge | PatchType::Apply)) {
        return Err(ApiError::new(
            415,
            "UnsupportedMediaType",
            "the body of the request was in an unknown format - accepted media types include: application/json-patch+json, application/merge-patch+json, application/apply-patch+yaml",
        ));
    }
    pt.ok_or_else(|| ApiError::unsupported_media_type(ct))
}

#[allow(clippy::too_many_arguments)]
async fn patch(
    s: &Arc<ApiServer>,
    info: &Arc<ResourceInfo>,
    ns: &str,
    name: &str,
    sub: Option<&str>,
    q: &Query,
    headers: &HeaderMap,
    body: Bytes,
) -> HandlerResult {
    let ct = headers.get("content-type").and_then(|h| h.to_str().ok()).unwrap_or("");
    let pt = patch_type(info, ct)?;
    let fv = FieldValidation::from_query(q)?;
    let mode = match sub {
        Some("status") => UpdateMode::Status,
        Some("approval") => UpdateMode::Approval,
        _ => UpdateMode::Main,
    };
    let key = info.key(ns, name);
    for _ in 0..5 {
        let old = match info.store(&s.storage).get(&key).await {
            Ok(o) => o,
            Err(StorageError::NotFound(_)) if pt == PatchType::Apply && mode == UpdateMode::Main => {
                // Server-side apply creates the object if it does not exist.
                let mut obj: Value =
                    serde_yaml::from_slice(&body).map_err(|e| ApiError::bad_request(format!("invalid apply patch: {e}")))?;
                obj.metadata_mut().insert("name".into(), json!(name));
                let created = create_object(s, info, ns, obj, dry_run(q), fv).await?;
                return Ok(json_response(201, &created));
            }
            Err(e) => return Err(storage_err(info, name, e)),
        };
        let mut patched = apply_patch(&old, pt, &body)?;
        // The patch may carry an explicit resourceVersion precondition.
        let explicit_rv = patched.resource_version() != old.resource_version();
        let expected = if explicit_rv {
            parse_rv(patched.resource_version()).map_err(ApiError::bad_request)?
        } else {
            parse_rv(old.resource_version()).ok().flatten()
        };
        patched.metadata_mut().insert("resourceVersion".into(), json!(old.resource_version()));
        match update_object(s, info, &old, patched, expected, mode, dry_run(q), fv).await {
            Ok(out) => return Ok(json_response(200, &out)),
            Err(e) if e.is_conflict() && !explicit_rv => continue,
            Err(e) => return Err(e),
        }
    }
    Err(ApiError::conflict_modified(&info.group, &info.plural, name))
}

// ---------------------------------------------------------------------------
// scale subresource (autoscaling/v1 Scale)
// ---------------------------------------------------------------------------

fn to_scale(info: &ResourceInfo, obj: &Value) -> Value {
    use crate::registry::{json_path_pointer, ScaleSelector};
    let paths = info.strategy.scale_paths();
    let selector = match &paths.selector {
        ScaleSelector::SpecMatchLabels => obj
            .pointer("/spec/selector/matchLabels")
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or(""))).collect::<Vec<_>>().join(","))
            .unwrap_or_default(),
        ScaleSelector::Path(Some(p)) => obj.pointer(&json_path_pointer(p)).and_then(Value::as_str).unwrap_or("").to_string(),
        ScaleSelector::Path(None) => String::new(),
    };
    // Workloads default `spec.replicas` to 1; custom resources report a missing value as 0.
    let missing = if matches!(paths.selector, ScaleSelector::SpecMatchLabels) { 1 } else { 0 };
    let mut status = json!({"replicas": obj.pointer(&json_path_pointer(&paths.status_replicas)).cloned().unwrap_or(json!(0))});
    if !selector.is_empty() {
        status["selector"] = json!(selector);
    } else if matches!(paths.selector, ScaleSelector::SpecMatchLabels) {
        status["selector"] = json!("");
    }
    json!({
        "kind": "Scale",
        "apiVersion": "autoscaling/v1",
        "metadata": {
            "name": obj.name(), "namespace": obj.namespace(), "uid": obj.uid(),
            "resourceVersion": obj.resource_version(), "creationTimestamp": obj.creation_timestamp(),
        },
        "spec": {"replicas": obj.pointer(&json_path_pointer(&paths.spec_replicas)).cloned().unwrap_or(json!(missing))},
        "status": status,
    })
}

/// Sets the value at a dotted JSON path, creating intermediate objects.
fn set_json_path(obj: &mut Value, path: &str, v: Value) {
    let mut cur = obj;
    let parts: Vec<&str> = path.split('.').filter(|p| !p.is_empty()).collect();
    for (i, p) in parts.iter().enumerate() {
        if !cur.is_object() {
            *cur = json!({});
        }
        let m = cur.as_object_mut().unwrap();
        if i == parts.len() - 1 {
            m.insert(p.to_string(), v);
            return;
        }
        cur = m.entry(p.to_string()).or_insert_with(|| json!({}));
    }
}

async fn get_scale(s: &Arc<ApiServer>, info: &Arc<ResourceInfo>, ns: &str, name: &str) -> HandlerResult {
    let obj = info.store(&s.storage).get(&info.key(ns, name)).await.map_err(|e| storage_err(info, name, e))?;
    Ok(json_response(200, &to_scale(info, &obj)))
}

async fn apply_scale(s: &Arc<ApiServer>, info: &Arc<ResourceInfo>, old: &Value, scale: &Value, dry: bool) -> HandlerResult {
    let replicas =
        scale.pointer("/spec/replicas").and_then(Value::as_i64).ok_or_else(|| ApiError::bad_request("spec.replicas is required"))?;
    if replicas < 0 {
        let e = FieldError::invalid("spec.replicas", replicas, "must be greater than or equal to 0");
        return Err(ApiError::invalid("autoscaling", "Scale", old.name(), &[e]));
    }
    let expected = parse_rv(scale.resource_version()).map_err(ApiError::bad_request)?;
    let mut new = old.clone();
    set_json_path(&mut new, &info.strategy.scale_paths().spec_replicas, json!(replicas));
    let out = update_object(s, info, old, new, expected, UpdateMode::Main, dry, FieldValidation::Ignore).await?;
    Ok(json_response(200, &to_scale(info, &out)))
}

async fn update_scale(
    s: &Arc<ApiServer>,
    info: &Arc<ResourceInfo>,
    ns: &str,
    name: &str,
    q: &Query,
    headers: &HeaderMap,
    body: Bytes,
) -> HandlerResult {
    let scale = decode_body(s, headers, &body)?;
    let old = info.store(&s.storage).get(&info.key(ns, name)).await.map_err(|e| storage_err(info, name, e))?;
    apply_scale(s, info, &old, &scale, dry_run(q)).await
}

async fn patch_scale(
    s: &Arc<ApiServer>,
    info: &Arc<ResourceInfo>,
    ns: &str,
    name: &str,
    q: &Query,
    headers: &HeaderMap,
    body: Bytes,
) -> HandlerResult {
    let ct = headers.get("content-type").and_then(|h| h.to_str().ok()).unwrap_or("");
    let pt = PatchType::from_content_type(ct).ok_or_else(|| ApiError::unsupported_media_type(ct))?;
    let old = info.store(&s.storage).get(&info.key(ns, name)).await.map_err(|e| storage_err(info, name, e))?;
    let mut scale = apply_patch(&to_scale(info, &old), pt, &body)?;
    scale.metadata_mut().remove("resourceVersion");
    apply_scale(s, info, &old, &scale, dry_run(q)).await
}

// ---------------------------------------------------------------------------
// custom subresources (binding, eviction, ...)
// ---------------------------------------------------------------------------

async fn create_subresource(
    s: &Arc<ApiServer>,
    info: &Arc<ResourceInfo>,
    ns: &str,
    name: &str,
    sub: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> HandlerResult {
    let mut body = decode_body(s, headers, &body)?;
    let obj = info.store(&s.storage).get(&info.key(ns, name)).await.map_err(|e| storage_err(info, name, e))?;
    let ctx = s.ctx();
    // Subresource creates (binding, eviction, token) are admitted like any create; the
    // request body is the object and the parent is the old object.
    let attrs = Attributes {
        operation: Operation::Create,
        resource: info,
        subresource: Some(sub),
        namespace: ns,
        name,
        old_object: Some(&obj),
        dry_run: false,
    };
    s.admission.admit(&attrs, &mut body, &ctx).await?;
    s.admission.validate(&attrs, &body, &ctx).await?;
    match info.strategy.create_subresource(&ctx, sub, &obj, body).await {
        Some(Ok(v)) => Ok(json_response(201, &v)),
        Some(Err(e)) => Err(e),
        None => Err(ApiError::method_not_allowed(format!("POST is not supported on {sub}"))),
    }
}

// ---------------------------------------------------------------------------
// delete
// ---------------------------------------------------------------------------

/// Removes an object from storage, running the strategy hooks.
pub(crate) async fn remove_object(s: &Arc<ApiServer>, info: &Arc<ResourceInfo>, obj: &Value) -> Result<Value, ApiError> {
    let ctx = s.ctx();
    info.strategy.before_delete(&ctx, obj).await?;
    let expected = parse_rv(obj.resource_version()).ok().flatten();
    let deleted = info
        .store(&s.storage)
        .delete(&info.key(obj.namespace(), obj.name()), expected)
        .await
        .map_err(|e| storage_err(info, obj.name(), e))?;
    info.strategy.after_delete(&ctx, &deleted).await;
    Ok(deleted)
}

/// `DeleteOptions` from body and query.
struct DeleteOptions {
    grace_period: Option<i64>,
    /// `Background` (default), `Foreground` or `Orphan`.
    propagation: Option<String>,
    precondition_uid: Option<String>,
    precondition_rv: Option<String>,
    dry_run: bool,
}

impl DeleteOptions {
    /// Parses query parameters and an (optional, JSON/YAML/protobuf) `DeleteOptions` body.
    fn parse(s: &ApiServer, q: &Query, headers: &HeaderMap, body: &[u8]) -> Result<Self, ApiError> {
        let b: Value = if body.iter().all(u8::is_ascii_whitespace) {
            json!({})
        } else {
            decode_body(s, headers, body).map_err(|e| ApiError::bad_request(format!("invalid DeleteOptions: {}", e.message)))?
        };
        Ok(Self {
            grace_period: b.get("gracePeriodSeconds").and_then(Value::as_i64).or_else(|| q.int("gracePeriodSeconds")),
            propagation: b
                .get("propagationPolicy")
                .and_then(Value::as_str)
                .map(String::from)
                .or_else(|| q.get("propagationPolicy").map(String::from))
                .or_else(|| match b.get("orphanDependents").and_then(Value::as_bool) {
                    Some(true) => Some("Orphan".into()),
                    Some(false) => Some("Background".into()),
                    None => None,
                }),
            precondition_uid: b.pointer("/preconditions/uid").and_then(Value::as_str).map(String::from),
            precondition_rv: b.pointer("/preconditions/resourceVersion").and_then(Value::as_str).map(String::from),
            dry_run: dry_run(q) || b.get("dryRun").and_then(Value::as_array).is_some_and(|a| a.iter().any(|x| x == "All")),
        })
    }
}

async fn delete_one(s: &Arc<ApiServer>, info: &Arc<ResourceInfo>, old: Value, opts: &DeleteOptions) -> Result<Value, ApiError> {
    let name = old.name().to_string();
    if let Some(uid) = &opts.precondition_uid {
        if uid != old.uid() {
            return Err(ApiError::conflict(
                &info.group,
                &info.plural,
                &name,
                &format!("Precondition failed: UID in precondition: {uid}, UID in object meta: {}", old.uid()),
            ));
        }
    }
    if let Some(rv) = &opts.precondition_rv {
        if rv != old.resource_version() {
            return Err(ApiError::conflict(
                &info.group,
                &info.plural,
                &name,
                &format!(
                    "Precondition failed: ResourceVersion in precondition: {rv}, ResourceVersion in object meta: {}",
                    old.resource_version()
                ),
            ));
        }
    }
    let ctx = s.ctx();
    let namespace = old.namespace().to_string();
    let attrs = Attributes {
        operation: Operation::Delete,
        resource: info,
        subresource: None,
        namespace: &namespace,
        name: &name,
        old_object: Some(&old),
        dry_run: opts.dry_run,
    };
    s.admission.validate(&attrs, &old, &ctx).await?;
    if opts.dry_run {
        return Ok(old);
    }
    let key = info.key(&namespace, &name);
    let expected = parse_rv(old.resource_version()).ok().flatten();

    // Graceful deletion (pods on a node): mark and let the node finish.
    if let Some(grace) = info.strategy.graceful_delete_seconds(&old, opts.grace_period) {
        if grace > 0 {
            if old.deletion_timestamp().is_some() {
                return Ok(old);
            }
            let mut marked = old.clone();
            let when = chrono::Utc::now() + chrono::Duration::seconds(grace);
            let m = marked.metadata_mut();
            m.insert("deletionTimestamp".into(), json!(when.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)));
            m.insert("deletionGracePeriodSeconds".into(), json!(grace));
            return info.store(&s.storage).update(&key, expected, marked).await.map_err(|e| storage_err(info, &name, e));
        }
    }
    // Propagation policy (garbage collector finalizers): Foreground waits for
    // dependents to be deleted first, Orphan removes their ownerReferences.
    let gc_finalizer = match opts.propagation.as_deref() {
        Some("Foreground") => Some(FOREGROUND_FINALIZER),
        Some("Orphan") => Some(ORPHAN_FINALIZER),
        Some("Background") | None => None,
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "propagationPolicy: Unsupported value: {other:?}: supported values: \"Orphan\", \"Foreground\", \"Background\""
            )))
        }
    };
    let mut finalizers = old.finalizers();
    if let Some(f) = gc_finalizer {
        if old.deletion_timestamp().is_none() && !finalizers.iter().any(|x| x == f) {
            finalizers.push(f.to_string());
        }
    }
    // Finalizers: mark for deletion; the object is removed when the last finalizer goes away.
    if !finalizers.is_empty() || !info.strategy.can_remove(&old) {
        if old.deletion_timestamp().is_some()
            && old.pointer("/metadata/deletionGracePeriodSeconds").and_then(Value::as_i64).unwrap_or(0) == 0
        {
            return Ok(old);
        }
        let mut marked = old.clone();
        let m = marked.metadata_mut();
        if !m.contains_key("deletionTimestamp") {
            m.insert("deletionTimestamp".into(), json!(now_rfc3339()));
        }
        m.insert("deletionGracePeriodSeconds".into(), json!(0));
        if !finalizers.is_empty() {
            m.insert("finalizers".into(), json!(finalizers));
        }
        info.strategy.on_delete_marked(&mut marked);
        return info.store(&s.storage).update(&key, expected, marked).await.map_err(|e| storage_err(info, &name, e));
    }
    remove_object(s, info, &old).await
}

async fn delete(
    s: &Arc<ApiServer>,
    info: &Arc<ResourceInfo>,
    ns: &str,
    name: &str,
    q: &Query,
    headers: &HeaderMap,
    body: Bytes,
) -> HandlerResult {
    let opts = DeleteOptions::parse(s, q, headers, &body)?;
    let old = info.store(&s.storage).get(&info.key(ns, name)).await.map_err(|e| storage_err(info, name, e))?;
    let out = delete_one(s, info, old, &opts).await?;
    Ok(json_response(200, &out))
}

async fn delete_collection(
    s: &Arc<ApiServer>,
    info: &Arc<ResourceInfo>,
    ns: &str,
    q: &Query,
    headers: &HeaderMap,
    body: Bytes,
) -> HandlerResult {
    let filter = Filter::from_query(q)?;
    let opts = DeleteOptions::parse(s, q, headers, &body)?;
    let (items, _) = info.store(&s.storage).list(&info.prefix(ns)).await.map_err(|e| storage_err(info, "", e))?;
    let mut deleted = vec![];
    for obj in items.into_iter().filter(|o| filter.matches(o)) {
        match delete_one(s, info, obj, &opts).await {
            Ok(d) => deleted.push(d),
            Err(e) if e.is_not_found() => {}
            Err(e) => return Err(e),
        }
    }
    Ok(json_response(
        200,
        &json!({
            "kind": format!("{}List", info.kind),
            "apiVersion": info.api_version(),
            "metadata": {"resourceVersion": s.storage.current_revision().to_string()},
            "items": deleted,
        }),
    ))
}

/// `deletecollection` across all namespaces, as the server itself.
pub(crate) async fn delete_all(s: &Arc<ApiServer>, info: &Arc<ResourceInfo>) -> Result<usize, ApiError> {
    let opts = DeleteOptions { grace_period: None, propagation: None, precondition_uid: None, precondition_rv: None, dry_run: false };
    let (items, _) = info.store(&s.storage).list(&info.prefix("")).await.map_err(|e| storage_err(info, "", e))?;
    for obj in items {
        match delete_one(s, info, obj, &opts).await {
            Ok(_) => {}
            Err(e) if e.is_not_found() => {}
            Err(e) => return Err(e),
        }
    }
    let (left, _) = info.store(&s.storage).list(&info.prefix("")).await.map_err(|e| storage_err(info, "", e))?;
    Ok(left.len())
}

/// Asks the resource's strategy where a CONNECT subresource lives (e.g. the kubelet).
pub(crate) async fn connect_url(
    s: &Arc<ApiServer>,
    info: &Arc<ResourceInfo>,
    ns: &str,
    name: &str,
    sub: &str,
    q: &Query,
) -> Result<String, ApiError> {
    let obj = info.store(&s.storage).get(&info.key(ns, name)).await.map_err(|e| storage_err(info, name, e))?;
    let raw: String = q.0.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect::<Vec<_>>().join("&");
    let raw = q.raw().map(String::from).unwrap_or(raw);
    match info.strategy.connect(&s.ctx(), sub, &obj, &raw).await {
        Some(r) => r,
        None => Err(ApiError::method_not_allowed(format!("{sub} is not supported"))),
    }
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
