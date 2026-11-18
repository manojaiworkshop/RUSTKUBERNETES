//! `NodeAffinity`: `spec.nodeSelector` and `spec.affinity.nodeAffinity`.
//!
//! Filter: `nodeSelector` and every `requiredDuringScheduling...` term list
//! (terms are ORed; expressions/fields within a term are ANDed; `matchFields`
//! supports `metadata.name`, which DaemonSets use to pin pods).
//! Score: sum of weights of matching `preferredDuringScheduling...` terms.

use std::collections::BTreeMap;

use k8s_api::core::v1::Pod;
use serde_json::Value;

use super::requirement_matches;
use crate::framework::{CycleState, NodeInfo, Normalize, Plugin, Status};

pub const NAME: &str = "NodeAffinity";

/// One `NodeSelectorTerm` against a node.
pub fn term_matches(term: &Value, labels: &BTreeMap<String, String>, node_name: &str) -> bool {
    let exprs = term["matchExpressions"].as_array().cloned().unwrap_or_default();
    let fields = term["matchFields"].as_array().cloned().unwrap_or_default();
    if exprs.is_empty() && fields.is_empty() {
        return false; // an empty term matches nothing
    }
    let field_map: BTreeMap<String, String> = [("metadata.name".to_string(), node_name.to_string())].into_iter().collect();
    exprs.iter().all(|r| requirement_matches(r, labels)) && fields.iter().all(|r| requirement_matches(r, &field_map))
}

/// Whether the pod's node selector and required node affinity accept the node.
pub fn required_matches(pod: &Pod, node: &NodeInfo) -> bool {
    let Some(spec) = pod.spec.as_ref() else { return true };
    let labels = node.labels();
    if let Some(sel) = &spec.node_selector {
        if !sel.iter().all(|(k, v)| labels.get(k) == Some(v)) {
            return false;
        }
    }
    let req =
        spec.affinity.as_ref().and_then(|a| a.pointer("/nodeAffinity/requiredDuringSchedulingIgnoredDuringExecution/nodeSelectorTerms"));
    match req.and_then(Value::as_array) {
        Some(terms) => terms.iter().any(|t| term_matches(t, &labels, node.name())),
        None => true,
    }
}

pub struct NodeAffinity;

impl Plugin for NodeAffinity {
    fn name(&self) -> &'static str {
        NAME
    }

    fn filter(&self, _: &CycleState, pod: &Pod, node: &NodeInfo) -> Status {
        if required_matches(pod, node) {
            Status::success()
        } else {
            Status::unresolvable("node(s) didn't match Pod's node affinity/selector")
        }
    }

    fn score(&self, _: &CycleState, pod: &Pod, node: &NodeInfo) -> Option<i64> {
        let prefs = pod
            .spec
            .as_ref()
            .and_then(|s| s.affinity.as_ref())
            .and_then(|a| a.pointer("/nodeAffinity/preferredDuringSchedulingIgnoredDuringExecution"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let labels = node.labels();
        Some(prefs.iter().filter(|p| term_matches(&p["preference"], &labels, node.name())).map(|p| p["weight"].as_i64().unwrap_or(0)).sum())
    }

    fn normalize(&self) -> Normalize {
        Normalize::Max
    }
}
