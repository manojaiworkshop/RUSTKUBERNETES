//! The apiextensions-apiserver controllers, run inside kube-apiserver:
//!
//! | Here | Upstream |
//! |------|----------|
//! | names + `NamesAccepted` / `Established=Installing` | `controller/status.NamingConditionController` |
//! | `Established=True` | `controller/establish.EstablishingController` |
//! | serving the custom resource (registry entries per served version) | `apiserver.crdHandler` + `customresource_discovery_controller` |
//! | `KubernetesAPIApprovalPolicyConformant` | `controller/apiapproval` |
//! | deleting instances, removing the cleanup finalizer | `controller/finalizer.CRDFinalizer` |
//! | local `APIService` per served group/version | `pkg/controlplane/controller/crdregistration` |

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use k8s_apimachinery::unstructured::ObjectExt;
use k8s_apimachinery::watch::EventType;
use k8s_apiserver::registry::ResourceBuilder;
use k8s_apiserver::storage::converting::{Converter, NoneConverter};
use k8s_apiserver::{ApiServer, ResourceInfo};
use serde_json::{json, Value};

use crate::apis::{self, str_at};
use crate::conversion::WebhookConverter;
use crate::registry::CustomResourceStrategy;

pub const PREFIX: &str = "/registry/apiextensions.k8s.io/customresourcedefinitions/";
const APISERVICE_PREFIX: &str = "/registry/apiregistration.k8s.io/apiservices/";
/// `crdregistration`: label value of APIServices created for CRD groups.
const AUTOMANAGED_LABEL: &str = "kube-aggregator.kubernetes.io/automanaged";

pub struct CrdController {
    server: Arc<ApiServer>,
    /// CRDs by name (the informer cache).
    cache: Mutex<BTreeMap<String, Value>>,
    /// CRDs whose instance cleanup is running.
    finalizing: Mutex<BTreeSet<String>>,
    /// resourceVersion of the CRD each registry entry was built from.
    served: Mutex<BTreeMap<String, String>>,
}

impl CrdController {
    /// Serves every established CRD, then follows changes in the background.
    pub async fn start(server: Arc<ApiServer>) -> Arc<Self> {
        let me = Arc::new(Self {
            server,
            cache: Mutex::new(BTreeMap::new()),
            finalizing: Mutex::new(BTreeSet::new()),
            served: Mutex::new(BTreeMap::new()),
        });
        let rev = me.relist().await;
        let w = me.clone();
        tokio::spawn(async move { w.watch_loop(rev).await });
        me
    }

    async fn relist(self: &Arc<Self>) -> u64 {
        let (items, rev) = match self.server.storage.list(PREFIX).await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!("listing CustomResourceDefinitions: {e}");
                return 0;
            }
        };
        let names: BTreeSet<String> = items.iter().map(|c| c.name().to_string()).collect();
        let stale: Vec<String> = self.cache.lock().unwrap().keys().filter(|n| !names.contains(*n)).cloned().collect();
        for n in stale {
            self.removed(&n).await;
        }
        for c in &items {
            self.cache.lock().unwrap().insert(c.name().to_string(), c.clone());
        }
        for c in items {
            self.clone().sync(c).await;
        }
        rev
    }

    async fn watch_loop(self: Arc<Self>, mut rev: u64) {
        loop {
            match self.server.storage.watch(PREFIX, rev).await {
                Ok(mut w) => {
                    while let Some(ev) = w.next().await {
                        let Ok(ev) = ev else { break };
                        let name = ev.object.name().to_string();
                        if ev.type_ == EventType::Deleted {
                            self.removed(&name).await;
                        } else {
                            self.cache.lock().unwrap().insert(name, ev.object.clone());
                            self.clone().sync(ev.object).await;
                        }
                    }
                }
                Err(e) => tracing::debug!("watching CustomResourceDefinitions: {e}"),
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            rev = self.relist().await;
        }
    }

    /// The CRD is gone from storage: stop serving it.
    async fn removed(&self, name: &str) {
        let crd = self.cache.lock().unwrap().remove(name);
        self.served.lock().unwrap().remove(name);
        self.server.registry.set_dynamic(name, vec![]);
        if let Some(crd) = crd {
            tracing::info!(crd = name, "stopped serving custom resource");
            self.unregister_apiservices(&crd).await;
        }
    }

    async fn sync(self: Arc<Self>, crd: Value) {
        let name = crd.name().to_string();
        if crd.deletion_timestamp().is_some() {
            // Keep serving (GET, LIST, DELETE) while instances are deleted.
            self.serve(&crd);
            if crd.finalizers().iter().any(|f| f == apis::CLEANUP_FINALIZER) && self.finalizing.lock().unwrap().insert(name.clone()) {
                let me = self.clone();
                tokio::spawn(async move {
                    me.finalize(&name).await;
                    me.finalizing.lock().unwrap().remove(&name);
                });
            }
            return;
        }
        let mut updated = crd.clone();
        self.naming(&mut updated);
        if apis::is_condition_true(&updated, apis::NAMES_ACCEPTED) && !apis::is_condition_true(&updated, apis::ESTABLISHED) {
            apis::set_condition(&mut updated, apis::ESTABLISHED, "True", "InitialNamesAccepted", "the initial names have been accepted");
        }
        api_approval(&mut updated);
        if updated != crd {
            match self.write(&name, &crd, updated).await {
                Some(written) => {
                    self.cache.lock().unwrap().insert(name.clone(), written.clone());
                    // The watch event of this write runs `sync` again and serves it.
                    return;
                }
                None => return,
            }
        }
        if apis::is_condition_true(&crd, apis::ESTABLISHED) {
            self.serve(&crd);
            self.register_apiservices(&crd).await;
        }
    }

    /// `NamingConditionController.calculateNamesAndConditions`
    fn naming(&self, crd: &mut Value) {
        let group = str_at(crd, "/spec/group").to_string();
        let me = crd.name().to_string();
        let (mut resources, mut kinds) = (BTreeSet::new(), BTreeSet::new());
        for (n, other) in self.cache.lock().unwrap().iter() {
            if *n == me || str_at(other, "/spec/group") != group {
                continue;
            }
            let a = &other["status"]["acceptedNames"];
            for k in ["plural", "singular"] {
                resources.insert(str_at(a, &format!("/{k}")).to_string());
            }
            for sn in a.get("shortNames").and_then(Value::as_array).into_iter().flatten() {
                resources.insert(sn.as_str().unwrap_or("").to_string());
            }
            kinds.insert(str_at(a, "/kind").to_string());
            kinds.insert(str_at(a, "/listKind").to_string());
        }
        let requested = crd["spec"]["names"].clone();
        let accepted = crd.pointer("/status/acceptedNames").cloned().unwrap_or(json!({}));
        let mut new_names = accepted.clone();
        if !new_names.is_object() {
            new_names = json!({});
        }
        let mut cond: Option<(&str, String)> = None;
        let fresh = |req: &str, acc: &str, used: &BTreeSet<String>| -> Result<(), String> {
            if req == acc || !used.contains(req) {
                Ok(())
            } else {
                Err(format!("{req:?} is already in use"))
            }
        };
        for (k, reason, used) in [
            ("plural", "PluralConflict", &resources),
            ("singular", "SingularConflict", &resources),
            ("kind", "KindConflict", &kinds),
            ("listKind", "ListKindConflict", &kinds),
        ] {
            let (req, acc) = (str_at(&requested, &format!("/{k}")), str_at(&accepted, &format!("/{k}")));
            match fresh(req, acc, used) {
                Ok(()) => new_names[k] = json!(req),
                Err(e) => cond = Some((reason, e)),
            }
            if k == "singular" {
                // short names are checked between singular and kind, like upstream
                let req_sn: Vec<String> = requested
                    .get("shortNames")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
                    .unwrap_or_default();
                let acc_sn: Vec<String> = accepted
                    .get("shortNames")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
                    .unwrap_or_default();
                if req_sn != acc_sn {
                    let errs: Vec<String> =
                        req_sn.iter().filter(|s| !acc_sn.contains(s)).filter_map(|s| fresh(s, "", &resources).err()).collect();
                    if errs.is_empty() {
                        if req_sn.is_empty() {
                            new_names.as_object_mut().unwrap().remove("shortNames");
                        } else {
                            new_names["shortNames"] = json!(req_sn);
                        }
                    } else {
                        cond =
                            Some(("ShortNamesConflict", if errs.len() == 1 { errs[0].clone() } else { format!("[{}]", errs.join(", ")) }));
                    }
                }
            }
        }
        match requested.get("categories").filter(|c| c.as_array().is_some_and(|a| !a.is_empty())) {
            Some(c) => new_names["categories"] = c.clone(),
            None => {
                new_names.as_object_mut().unwrap().remove("categories");
            }
        }
        if !crd["status"].is_object() {
            crd["status"] = json!({});
        }
        crd["status"]["acceptedNames"] = new_names;
        let names_ok = cond.is_none();
        match cond {
            None => apis::set_condition(crd, apis::NAMES_ACCEPTED, "True", "NoConflicts", "no conflicts found"),
            Some((reason, msg)) => apis::set_condition(crd, apis::NAMES_ACCEPTED, "False", reason, &msg),
        }
        let established = apis::is_condition_true(crd, apis::ESTABLISHED);
        if !established {
            if names_ok {
                apis::set_condition(crd, apis::ESTABLISHED, "False", "Installing", "the initial names have been accepted");
            } else if apis::condition(crd, apis::ESTABLISHED).is_none() {
                apis::set_condition(crd, apis::ESTABLISHED, "False", "NotAccepted", "not all names are accepted");
            }
        }
    }

    /// Writes the CRD's status, returning the stored object (or `None` on conflict).
    async fn write(&self, name: &str, old: &Value, new: Value) -> Option<Value> {
        let key = format!("{PREFIX}{name}");
        let rv = old.resource_version().parse().ok();
        match self.server.storage.update(&key, rv, new).await {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::debug!(crd = name, "status update: {e}");
                None
            }
        }
    }

    /// Registers one REST resource per served version (`crdHandler.getOrCreateServingInfoFor`).
    fn serve(&self, crd: &Value) {
        let name = crd.name().to_string();
        let rv = crd.resource_version().to_string();
        if self.served.lock().unwrap().get(&name) == Some(&rv) {
            return;
        }
        let current = self.server.registry.dynamic(&name);
        let crd_arc = Arc::new(crd.clone());
        let infos = build_infos(&crd_arc, &self.server.storage);
        if current.is_empty() && !infos.is_empty() {
            tracing::info!(crd = %name, versions = ?apis::served_versions(crd), "serving custom resource");
        }
        self.server.registry.set_dynamic(&name, infos);
        self.served.lock().unwrap().insert(name, rv);
    }

    /// `CRDFinalizer.sync`: deletes all instances, then removes the finalizer.
    async fn finalize(self: &Arc<Self>, name: &str) {
        loop {
            let key = format!("{PREFIX}{name}");
            let Ok(crd) = self.server.storage.get(&key).await else { return };
            if crd.deletion_timestamp().is_none() || !crd.finalizers().iter().any(|f| f == apis::CLEANUP_FINALIZER) {
                return;
            }
            let mut updated = crd.clone();
            apis::set_condition(
                &mut updated,
                apis::TERMINATING,
                "True",
                "InstanceDeletionInProgress",
                "CustomResource deletion is in progress",
            );
            let Some(crd) = self.write(name, &crd, updated).await else {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            };
            let infos = self.server.registry.dynamic(name);
            let result = match infos.first() {
                // Never served: nothing can exist.
                None => Ok(0),
                Some(info) => self.server.delete_all_internal(info).await,
            };
            let mut updated = crd.clone();
            match result {
                Ok(0) => {
                    apis::set_condition(&mut updated, apis::TERMINATING, "False", "InstanceDeletionCompleted", "removed all instances");
                    let fins: Vec<String> = crd.finalizers().into_iter().filter(|f| f != apis::CLEANUP_FINALIZER).collect();
                    updated.metadata_mut().insert("finalizers".into(), json!(fins));
                    if fins.is_empty() {
                        updated.metadata_mut().remove("finalizers");
                    }
                    let Some(done) = self.write(name, &crd, updated).await else { continue };
                    if done.finalizers().is_empty() {
                        let rv = done.resource_version().parse().ok();
                        if let Err(e) = self.server.storage.delete(&key, rv).await {
                            tracing::debug!(crd = name, "removing finalized CRD: {e}");
                            continue;
                        }
                    }
                    tracing::info!(crd = name, "removed all instances, CustomResourceDefinition deleted");
                    return;
                }
                Ok(n) => {
                    apis::set_condition(
                        &mut updated,
                        apis::TERMINATING,
                        "True",
                        "InstanceDeletionCheck",
                        &format!("could not confirm zero CustomResources remaining: {n} remaining"),
                    );
                }
                Err(e) => {
                    apis::set_condition(
                        &mut updated,
                        apis::TERMINATING,
                        "True",
                        "InstanceDeletionFailed",
                        &format!("could not issue all deletes: {}", e.message),
                    );
                }
            }
            self.write(name, &crd, updated).await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    /// `crdregistration`: an `APIService` (priority 1000/100) per served group/version.
    async fn register_apiservices(&self, crd: &Value) {
        let group = str_at(crd, "/spec/group");
        for v in apis::served_versions(crd) {
            let name = format!("{v}.{group}");
            if self.server.storage.get(&format!("{APISERVICE_PREFIX}{name}")).await.is_ok() {
                continue;
            }
            let obj = json!({
                "apiVersion": "apiregistration.k8s.io/v1", "kind": "APIService",
                "metadata": {"name": name, "labels": {AUTOMANAGED_LABEL: "true"}},
                "spec": {"group": group, "version": v, "groupPriorityMinimum": 1000, "versionPriority": 100},
            });
            if let Err(e) = self.server.create_internal("apiregistration.k8s.io", "v1", "apiservices", "", obj).await {
                if !e.is_already_exists() {
                    tracing::warn!(apiservice = %name, "registering: {}", e.message);
                }
            }
        }
    }

    /// Removes automanaged APIServices no remaining CRD serves.
    async fn unregister_apiservices(&self, crd: &Value) {
        let group = str_at(crd, "/spec/group");
        let still: BTreeSet<String> =
            self.cache.lock().unwrap().values().filter(|c| str_at(c, "/spec/group") == group).flat_map(apis::served_versions).collect();
        for v in apis::served_versions(crd) {
            if still.contains(&v) {
                continue;
            }
            let key = format!("{APISERVICE_PREFIX}{v}.{group}");
            if let Ok(a) = self.server.storage.get(&key).await {
                if str_at(&a, "/metadata/labels/kube-aggregator.kubernetes.io~1automanaged") == "true" {
                    let _ = self.server.storage.delete(&key, None).await;
                }
            }
        }
    }
}

/// `apiapproval.calculateCondition`: only for protected groups.
fn api_approval(crd: &mut Value) {
    if !apis::is_protected_group(str_at(crd, "/spec/group")) {
        apis::remove_condition(crd, apis::API_APPROVAL_CONDITION);
        return;
    }
    let ann = crd.pointer("/metadata/annotations").cloned().unwrap_or(Value::Null);
    let (state, reason) = apis::api_approval_state(&ann);
    let (status, r) = match state {
        "Approved" => ("True", "ApprovedAnnotation"),
        "Unapproved" => ("False", "UnapprovedAnnotation"),
        "Invalid" => ("False", "InvalidAnnotation"),
        "Missing" => ("False", "MissingAnnotation"),
        _ => ("Unknown", "UnknownAnnotation"),
    };
    apis::set_condition(crd, apis::API_APPROVAL_CONDITION, status, r, &reason);
}

/// One [`ResourceInfo`] per served version, using the accepted names.
pub fn build_infos(crd: &Arc<Value>, storage: &Arc<dyn k8s_apiserver::Storage>) -> Vec<Arc<ResourceInfo>> {
    let group = str_at(crd, "/spec/group").to_string();
    let names = &crd["status"]["acceptedNames"];
    let (plural, kind) = (str_at(names, "/plural"), str_at(names, "/kind"));
    if plural.is_empty() || kind.is_empty() {
        return vec![];
    }
    let singular = match str_at(names, "/singular") {
        "" => kind.to_lowercase(),
        s => s.to_string(),
    };
    let list = |k: &str| -> Vec<String> {
        names.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default()
    };
    let (short, cats) = (list("shortNames"), list("categories"));
    let namespaced = str_at(crd, "/spec/scope") == "Namespaced";
    let Some(storage_ver) = apis::storage_version(crd) else { return vec![] };
    let inner: Arc<dyn Converter> = match str_at(crd, "/spec/conversion/strategy") {
        "Webhook" => Arc::new(WebhookConverter::new(crd.clone(), storage.clone())),
        _ => Arc::new(NoneConverter),
    };
    let converter: Arc<dyn Converter> = Arc::new(crate::registry::SchemaCoercingConverter { inner, crd: crd.clone() });
    apis::served_versions(crd)
        .into_iter()
        .map(|v| {
            let ver = apis::version(crd, &v).cloned().unwrap_or(Value::Null);
            let mut subs = vec![];
            if ver.pointer("/subresources/status").is_some_and(Value::is_object) {
                subs.push("status");
            }
            if ver.pointer("/subresources/scale").is_some_and(Value::is_object) {
                subs.push("scale");
            }
            ResourceBuilder::new(&group, &v, kind, plural, namespaced)
                .singular(&singular)
                .short_names(&short.iter().map(String::as_str).collect::<Vec<_>>())
                .categories(&cats.iter().map(String::as_str).collect::<Vec<_>>())
                .subresources(&subs)
                .strategy(Arc::new(CustomResourceStrategy::new(crd.clone(), &v)))
                .storage_prefix(&format!("{group}/{plural}"))
                .storage_version(&format!("{group}/{storage_ver}"), converter.clone())
                .build()
        })
        .collect()
}
