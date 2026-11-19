//! Port of `plugin/pkg/admission/noderestriction` (`NodeRestriction`): limits
//! what a kubelet (`system:node:<name>`) may write to objects that concern
//! its own node.
//!
//! | Resource | A node may |
//! |----------|------------|
//! | nodes, nodes/status | create/update only its own Node; never change its taints or `configSource`; set/modify only kubelet labels (no `node-restriction.kubernetes.io/`, no unknown `kubernetes.io`/`k8s.io` labels) |
//! | pods | create only mirror pods bound to itself (owned by its Node, no secrets/configmaps/PVCs/ServiceAccount); delete only pods bound to itself |
//! | pods/status | update only pods bound to itself, without changing labels |
//! | pods/eviction | evict only pods bound to itself |
//! | serviceaccounts/token | only tokens bound to a pod (with UID) running on itself |
//! | leases | only its own Lease in `kube-node-lease` |
//! | persistentvolumeclaims/status | only `status.capacity` / `status.conditions` |
//! | csinodes | only its own CSINode |
//!
//! Requests from anyone else pass through untouched. Enable it with
//! `--enable-admission-plugins=NodeRestriction` (kubeadm does).

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use k8s_apimachinery::errors::ApiError;
use k8s_apiserver::admission::{AdmissionPlugin, Attributes, Operation};
use k8s_apiserver::authentication::current_user;
use k8s_apiserver::registry::StrategyContext;
use serde_json::Value;

use kube_kubelet::apis::{in_namespace, is_kubelet_label, is_kubernetes_label, label_namespace};

use crate::auth::node::NAMESPACE_NODE_LEASE;
use crate::auth::nodeidentifier::node_identity;

pub const PLUGIN_NAME: &str = "NodeRestriction";
/// `api.MirrorPodAnnotationKey`
const MIRROR_POD_ANNOTATION: &str = "kubernetes.io/config.mirror";
/// `v1.LabelNamespaceNodeRestriction`
const LABEL_NAMESPACE_NODE_RESTRICTION: &str = "node-restriction.kubernetes.io";

pub struct NodeRestriction;

/// `admission.NewForbidden`: `<resource> "<name>" is forbidden: <reason>`.
fn forbidden(a: &Attributes<'_>, obj: Option<&Value>, msg: impl std::fmt::Display) -> ApiError {
    let mut name = a.name.to_string();
    if name.is_empty() {
        name = obj
            .and_then(|o| {
                let m = o.get("metadata")?;
                m.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).or_else(|| m.get("generateName").and_then(Value::as_str))
            })
            .unwrap_or("Unknown")
            .to_string();
    }
    ApiError::forbidden(format!("{} {name:?} is forbidden: {msg}", a.resource.group_resource()))
}

fn s<'a>(v: &'a Value, ptr: &str) -> &'a str {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or("")
}

fn labels(v: Option<&Value>) -> BTreeMap<String, String> {
    v.and_then(|o| o.pointer("/metadata/labels"))
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect())
        .unwrap_or_default()
}

/// `getModifiedLabels`: keys added, removed or changed.
fn modified_labels(a: &BTreeMap<String, String>, b: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (k, v) in a {
        if b.get(k) != Some(v) {
            out.insert(k.clone());
        }
    }
    for (k, v) in b {
        if a.get(k) != Some(v) {
            out.insert(k.clone());
        }
    }
    out
}

/// `getForbiddenLabels`
fn forbidden_labels(modified: &BTreeSet<String>) -> Vec<String> {
    modified
        .iter()
        .filter(|l| in_namespace(label_namespace(l), LABEL_NAMESPACE_NODE_RESTRICTION) || (is_kubernetes_label(l) && !is_kubelet_label(l)))
        .cloned()
        .collect()
}

/// Taints compared like `apiequality.Semantic.DeepEqual` (missing == empty).
fn taints(v: Option<&Value>) -> Value {
    v.and_then(|o| o.pointer("/spec/taints")).filter(|t| t.as_array().is_some_and(|a| !a.is_empty())).cloned().unwrap_or(Value::Null)
}

impl NodeRestriction {
    async fn get(&self, ctx: &StrategyContext<'_>, plural: &str, ns: &str, name: &str) -> Result<Option<Value>, String> {
        let info = ctx.registry.lookup("", "v1", plural).ok_or_else(|| format!("{plural} not registered"))?;
        match ctx.storage.get(&info.key(ns, name)).await {
            Ok(v) => Ok(Some(v)),
            Err(k8s_apiserver::storage::StorageError::NotFound(_)) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    /// `admitNode`
    fn admit_node(&self, node: &str, a: &Attributes<'_>, obj: &Value) -> Result<(), ApiError> {
        let requested = if a.name.is_empty() { s(obj, "/metadata/name") } else { a.name };
        if requested != node {
            return Err(forbidden(a, Some(obj), format!("node {node:?} is not allowed to modify node {requested:?}")));
        }
        match a.operation {
            Operation::Create => {
                if obj.pointer("/spec/configSource").is_some_and(|c| !c.is_null()) {
                    return Err(forbidden(
                        a,
                        Some(obj),
                        format!("node {node:?} is not allowed to create pods with a non-nil configSource"),
                    ));
                }
                let bad = forbidden_labels(&modified_labels(&labels(Some(obj)), &BTreeMap::new()));
                if !bad.is_empty() {
                    return Err(forbidden(
                        a,
                        Some(obj),
                        format!("node {node:?} is not allowed to set the following labels: {}", bad.join(", ")),
                    ));
                }
            }
            Operation::Update => {
                let old = a.old_object;
                let new_cs = obj.pointer("/spec/configSource").filter(|c| !c.is_null());
                if new_cs.is_some() && new_cs != old.and_then(|o| o.pointer("/spec/configSource")) {
                    return Err(forbidden(
                        a,
                        Some(obj),
                        format!("node {node:?} is not allowed to update configSource to a new non-nil configSource"),
                    ));
                }
                if taints(Some(obj)) != taints(old) {
                    return Err(forbidden(a, Some(obj), format!("node {node:?} is not allowed to modify taints")));
                }
                let bad = forbidden_labels(&modified_labels(&labels(Some(obj)), &labels(old)));
                if !bad.is_empty() {
                    return Err(forbidden(a, Some(obj), format!("is not allowed to modify labels: {}", bad.join(", "))));
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// `admitPodCreate`: mirror pods only.
    async fn admit_pod_create(&self, node: &str, a: &Attributes<'_>, pod: &Value, ctx: &StrategyContext<'_>) -> Result<(), ApiError> {
        let f = |m: String| Err(forbidden(a, Some(pod), m));
        if pod.pointer(&format!("/metadata/annotations/{}", MIRROR_POD_ANNOTATION.replace('/', "~1"))).is_none() {
            return f(format!("pod does not have \"{MIRROR_POD_ANNOTATION}\" annotation, node {node:?} can only create mirror pods"));
        }
        if s(pod, "/spec/nodeName") != node {
            return f(format!("node {node:?} can only create pods with spec.nodeName set to itself"));
        }
        let owners = pod.pointer("/metadata/ownerReferences").and_then(Value::as_array).cloned().unwrap_or_default();
        match owners.len() {
            0 => return f(format!("node {node:?} can only create pods with an owner reference set to itself")),
            1 => {}
            _ => return f(format!("node {node:?} can only create pods with a single owner reference set to itself")),
        }
        let o = &owners[0];
        if s(o, "/apiVersion") != "v1" || s(o, "/kind") != "Node" || s(o, "/name") != node {
            return f(format!("node {node:?} can only create pods with an owner reference set to itself"));
        }
        if o.get("controller").and_then(Value::as_bool) != Some(true) {
            return f(format!("node {node:?} can only create pods with a controller owner reference set to itself"));
        }
        if o.get("blockOwnerDeletion").and_then(Value::as_bool) == Some(true) {
            return f(format!("node {node:?} must not set blockOwnerDeletion on an owner reference"));
        }
        match self.get(ctx, "nodes", "", node).await {
            Ok(Some(n)) => {
                let uid = s(&n, "/metadata/uid");
                if s(o, "/uid") != uid {
                    return f(format!("node {node} UID mismatch: expected {uid} got {}", s(o, "/uid")));
                }
            }
            Ok(None) => return Err(ApiError::not_found("", "nodes", node)),
            Err(e) => return f(format!("error looking up node {node} to verify uid: {e}")),
        }
        let refs = crate::auth::node::graph::pod_refs(pod);
        if !refs.service_account.is_empty() {
            return f(format!("node {node:?} can not create pods that reference a service account"));
        }
        if !refs.secrets.is_empty() {
            return f(format!("node {node:?} can not create pods that reference secrets"));
        }
        if !refs.config_maps.is_empty() {
            return f(format!("node {node:?} can not create pods that reference configmaps"));
        }
        if !refs.pvcs.is_empty() {
            return f(format!("node {node:?} can not create pods that reference persistentvolumeclaims"));
        }
        Ok(())
    }

    /// `admitPod`, `admitPodStatus`, `admitPodEviction`.
    async fn admit_pod(&self, node: &str, a: &Attributes<'_>, obj: &Value, ctx: &StrategyContext<'_>) -> Result<(), ApiError> {
        match (a.subresource.unwrap_or(""), a.operation) {
            ("", Operation::Create) => self.admit_pod_create(node, a, obj, ctx).await,
            ("", Operation::Delete) => {
                let Some(existing) = a.old_object else { return Err(ApiError::not_found("", "pods", a.name)) };
                if s(existing, "/spec/nodeName") != node {
                    return Err(forbidden(a, Some(obj), format!("node {node:?} can only delete pods with spec.nodeName set to itself")));
                }
                Ok(())
            }
            ("", op) => Err(forbidden(
                a,
                Some(obj),
                format!("unexpected operation {:?}, node {node:?} can only create and delete mirror pods", op.as_str()),
            )),
            ("status", Operation::Update) => {
                let old = a.old_object;
                if old.map(|o| s(o, "/spec/nodeName")) != Some(node) {
                    return Err(forbidden(
                        a,
                        Some(obj),
                        format!("node {node:?} can only update pod status for pods with spec.nodeName set to itself"),
                    ));
                }
                if labels(Some(obj)) != labels(old) {
                    return Err(forbidden(a, Some(obj), format!("node {node:?} cannot update labels through pod status")));
                }
                Ok(())
            }
            ("status", op) => Err(forbidden(a, Some(obj), format!("unexpected operation {:?}", op.as_str()))),
            ("eviction", Operation::Create) => {
                let pod_name = if a.name.is_empty() { s(obj, "/metadata/name") } else { a.name };
                if pod_name.is_empty() {
                    return Err(ApiError::bad_request("could not determine pod from request data"));
                }
                match self.get(ctx, "pods", a.namespace, pod_name).await {
                    Ok(Some(p)) if s(&p, "/spec/nodeName") == node => Ok(()),
                    Ok(Some(_)) => {
                        Err(forbidden(a, Some(obj), format!("node {node} can only evict pods with spec.nodeName set to itself")))
                    }
                    Ok(None) => Err(ApiError::not_found("", "pods", pod_name)),
                    Err(e) => Err(forbidden(a, Some(obj), e)),
                }
            }
            ("eviction", op) => Err(forbidden(a, Some(obj), format!("unexpected operation {}", op.as_str()))),
            (sub, _) => {
                Err(forbidden(a, Some(obj), format!("unexpected pod subresource {sub:?}, only 'status' and 'eviction' are allowed")))
            }
        }
    }

    /// `admitServiceAccount`: tokens must be bound to a pod on this node.
    async fn admit_token(&self, node: &str, a: &Attributes<'_>, tr: &Value, ctx: &StrategyContext<'_>) -> Result<(), ApiError> {
        if a.operation != Operation::Create || a.subresource != Some("token") {
            return Ok(());
        }
        let r = tr.pointer("/spec/boundObjectRef").filter(|r| r.is_object());
        let Some(r) = r.filter(|r| s(r, "/apiVersion") == "v1" && s(r, "/kind") == "Pod" && !s(r, "/name").is_empty()) else {
            return Err(forbidden(a, Some(tr), "node requested token not bound to a pod"));
        };
        let uid = s(r, "/uid");
        if uid.is_empty() {
            return Err(forbidden(a, Some(tr), "node requested token with a pod binding without a uid"));
        }
        let pod = match self.get(ctx, "pods", a.namespace, s(r, "/name")).await {
            Ok(Some(p)) => p,
            Ok(None) => return Err(ApiError::not_found("", "pods", s(r, "/name"))),
            Err(e) => return Err(forbidden(a, Some(tr), e)),
        };
        if s(&pod, "/metadata/uid") != uid {
            return Err(forbidden(
                a,
                Some(tr),
                format!(
                    "the UID in the bound object reference ({uid}) does not match the UID in record. The object might have been deleted and then recreated"
                ),
            ));
        }
        if s(&pod, "/spec/nodeName") != node {
            return Err(forbidden(a, Some(tr), "node requested token bound to a pod scheduled on a different node"));
        }
        Ok(())
    }

    /// `admitLease`
    fn admit_lease(&self, node: &str, a: &Attributes<'_>, obj: &Value) -> Result<(), ApiError> {
        if a.namespace != NAMESPACE_NODE_LEASE {
            return Err(forbidden(a, Some(obj), format!("can only access leases in the {NAMESPACE_NODE_LEASE:?} system namespace")));
        }
        let name = if a.operation == Operation::Create { s(obj, "/metadata/name") } else { a.name };
        if name != node {
            return Err(forbidden(a, Some(obj), "can only access node lease with the same name as the requesting node"));
        }
        Ok(())
    }

    /// `admitPVCStatus`: only capacity and conditions may change.
    fn admit_pvc(&self, node: &str, a: &Attributes<'_>, obj: &Value) -> Result<(), ApiError> {
        if a.subresource != Some("status") {
            return Err(forbidden(a, Some(obj), "may only update PVC status"));
        }
        if a.operation != Operation::Update {
            return Err(forbidden(a, Some(obj), format!("unexpected operation {:?}", a.operation.as_str())));
        }
        let strip = |v: &Value| {
            let mut v = v.clone();
            if let Some(m) = v.get_mut("metadata").and_then(Value::as_object_mut) {
                m.remove("resourceVersion");
            }
            if let Some(st) = v.get_mut("status").and_then(Value::as_object_mut) {
                st.remove("capacity");
                st.remove("conditions");
            }
            v
        };
        if a.old_object.map(strip) != Some(strip(obj)) {
            return Err(forbidden(
                a,
                Some(obj),
                format!("node {node:?} is not allowed to update fields other than status.capacity and status.conditions"),
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl AdmissionPlugin for NodeRestriction {
    fn name(&self) -> &str {
        PLUGIN_NAME
    }

    async fn validate(&self, a: &Attributes<'_>, obj: &Value, ctx: &StrategyContext<'_>) -> Result<(), ApiError> {
        let Some(user) = current_user() else { return Ok(()) };
        let (node, is_node) = node_identity(&user);
        // Our job is just to restrict nodes.
        if !is_node {
            return Ok(());
        }
        if node.is_empty() {
            return Err(forbidden(a, Some(obj), format!("could not determine node from user {:?}", user.name)));
        }
        let r = a.resource;
        match (r.group.as_str(), r.plural.as_str()) {
            ("", "pods") => self.admit_pod(&node, a, obj, ctx).await,
            ("", "nodes") => self.admit_node(&node, a, obj),
            ("", "persistentvolumeclaims") => self.admit_pvc(&node, a, obj),
            ("", "serviceaccounts") => self.admit_token(&node, a, obj, ctx).await,
            ("coordination.k8s.io", "leases") => self.admit_lease(&node, a, obj),
            ("storage.k8s.io", "csinodes") => {
                let name = if a.operation == Operation::Create { s(obj, "/metadata/name") } else { a.name };
                if name != node {
                    return Err(forbidden(a, Some(obj), "can only access CSINode with the same name as the requesting node"));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_rules() {
        let set = |ls: &[&str]| ls.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
        assert!(forbidden_labels(&set(&[
            "kubernetes.io/hostname",
            "kubernetes.io/os",
            "node.kubernetes.io/exclude-from-external-load-balancers",
            "example.com/team",
            "kubelet.kubernetes.io/x"
        ]))
        .is_empty());
        assert_eq!(
            forbidden_labels(&set(&[
                "node-role.kubernetes.io/control-plane",
                "node-restriction.kubernetes.io/secure",
                "foo.k8s.io/bar",
                "a.node-restriction.kubernetes.io/b"
            ])),
            vec![
                "a.node-restriction.kubernetes.io/b",
                "foo.k8s.io/bar",
                "node-restriction.kubernetes.io/secure",
                "node-role.kubernetes.io/control-plane"
            ]
        );
        let a: BTreeMap<String, String> = [("x".to_string(), "1".to_string())].into();
        let b: BTreeMap<String, String> = [("x".to_string(), "2".to_string()), ("y".to_string(), "1".to_string())].into();
        assert_eq!(modified_labels(&a, &b), set(&["x", "y"]));
    }
}
