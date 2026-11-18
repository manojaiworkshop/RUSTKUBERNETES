//! In-tree plugins (`pkg/scheduler/framework/plugins`) and the default plugin set.

pub mod interpodaffinity;
pub mod node;
pub mod nodeaffinity;
pub mod noderesources;
pub mod podtopologyspread;

use std::collections::BTreeMap;
use std::sync::Arc;

use k8s_api::core::v1::{Taint, Toleration};
use serde_json::Value;

use super::Plugin;

/// `(name, score weight)` of the default plugins, in filter order
/// (`pkg/scheduler/apis/config/v1/default_plugins.go`). Weight 0 = filter only.
pub const DEFAULT_PLUGINS: &[(&str, i64)] = &[
    (node::SCHEDULING_GATES, 0),
    (node::NODE_UNSCHEDULABLE, 0),
    (node::NODE_NAME, 0),
    (node::TAINT_TOLERATION, 3),
    (nodeaffinity::NAME, 2),
    (node::NODE_PORTS, 0),
    (noderesources::FIT, 1),
    (podtopologyspread::NAME, 2),
    (interpodaffinity::NAME, 2),
    (noderesources::BALANCED_ALLOCATION, 1),
    (node::IMAGE_LOCALITY, 1),
];

/// Instantiates an in-tree plugin by name with optional `pluginConfig` args.
pub fn new_plugin(name: &str, args: Option<&Value>) -> Option<Arc<dyn Plugin>> {
    Some(match name {
        node::SCHEDULING_GATES => Arc::new(node::SchedulingGates),
        node::NODE_UNSCHEDULABLE => Arc::new(node::NodeUnschedulable),
        node::NODE_NAME => Arc::new(node::NodeName),
        node::NODE_PORTS => Arc::new(node::NodePorts),
        node::TAINT_TOLERATION => Arc::new(node::TaintToleration),
        node::IMAGE_LOCALITY => Arc::new(node::ImageLocality),
        nodeaffinity::NAME => Arc::new(nodeaffinity::NodeAffinity),
        noderesources::FIT => Arc::new(noderesources::Fit::from_args(args)),
        noderesources::BALANCED_ALLOCATION => Arc::new(noderesources::BalancedAllocation),
        podtopologyspread::NAME => Arc::new(podtopologyspread::PodTopologySpread),
        interpodaffinity::NAME => Arc::new(interpodaffinity::InterPodAffinity),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// `Toleration.ToleratesTaint`
pub fn tolerates(t: &Toleration, taint: &Taint) -> bool {
    if let Some(e) = t.effect.as_deref().filter(|e| !e.is_empty()) {
        if e != taint.effect {
            return false;
        }
    }
    let key = t.key.as_deref().unwrap_or("");
    match t.operator.as_deref().unwrap_or("Equal") {
        "Exists" => key.is_empty() || key == taint.key,
        _ => key == taint.key && t.value.as_deref().unwrap_or("") == taint.value.as_deref().unwrap_or(""),
    }
}

/// One `matchExpressions` requirement against a label map (`In`, `NotIn`, `Exists`,
/// `DoesNotExist`, `Gt`, `Lt`).
pub fn requirement_matches(req: &Value, labels: &BTreeMap<String, String>) -> bool {
    let key = req["key"].as_str().unwrap_or("");
    let values: Vec<&str> = req["values"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let v = labels.get(key);
    match req["operator"].as_str().unwrap_or("") {
        "In" => v.is_some_and(|v| values.contains(&v.as_str())),
        "NotIn" => v.is_none_or(|v| !values.contains(&v.as_str())),
        "Exists" => v.is_some(),
        "DoesNotExist" => v.is_none(),
        "Gt" => v.and_then(|v| v.parse::<i64>().ok()).zip(values.first().and_then(|x| x.parse::<i64>().ok())).is_some_and(|(a, b)| a > b),
        "Lt" => v.and_then(|v| v.parse::<i64>().ok()).zip(values.first().and_then(|x| x.parse::<i64>().ok())).is_some_and(|(a, b)| a < b),
        _ => false,
    }
}

/// `metav1.LabelSelector` (as JSON) matching; `None`/null selects nothing.
pub fn label_selector_matches(sel: Option<&Value>, labels: &BTreeMap<String, String>) -> bool {
    let Some(sel) = sel.filter(|s| !s.is_null()) else { return false };
    let ml_ok = sel["matchLabels"].as_object().is_none_or(|m| m.iter().all(|(k, v)| labels.get(k).map(String::as_str) == v.as_str()));
    let me_ok = sel["matchExpressions"].as_array().is_none_or(|a| a.iter().all(|r| requirement_matches(r, labels)));
    ml_ok && me_ok
}
