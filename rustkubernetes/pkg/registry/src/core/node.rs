//! Port of `pkg/registry/core/node`.

use k8s_apimachinery::unstructured::ObjectExt;
use k8s_apiserver::table::{age_cell, Column};
use serde_json::{json, Value};

use crate::generic::{str_at, SimpleStrategy};

pub fn strategy() -> SimpleStrategy {
    SimpleStrategy::new(true).table(columns, cells)
}

fn columns() -> Vec<Column> {
    vec![
        Column::new("Status", "string", "The status of the node"),
        Column::new("Roles", "string", "The roles of the node"),
        Column::new("Age", "string", ""),
        Column::new("Version", "string", "Kubelet Version reported by the node."),
        Column::new("Internal-IP", "string", "").wide(),
        Column::new("External-IP", "string", "").wide(),
        Column::new("OS-Image", "string", "").wide(),
        Column::new("Kernel-Version", "string", "").wide(),
        Column::new("Container-Runtime", "string", "").wide(),
    ]
}

/// `Ready`, `NotReady`, `Unknown` (+ `,SchedulingDisabled`).
pub fn node_status(node: &Value) -> String {
    let ready = node
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .and_then(|cs| cs.iter().find(|c| c["type"] == "Ready"))
        .map(|c| match c["status"].as_str() {
            Some("True") => "Ready",
            Some("False") => "NotReady",
            _ => "Unknown",
        })
        .unwrap_or("Unknown");
    let mut s = ready.to_string();
    if node.pointer("/spec/unschedulable").and_then(Value::as_bool) == Some(true) {
        s.push_str(",SchedulingDisabled");
    }
    s
}

fn address(node: &Value, type_: &str) -> String {
    node.pointer("/status/addresses")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().find(|x| x["type"] == type_))
        .and_then(|x| x["address"].as_str())
        .unwrap_or("<none>")
        .to_string()
}

fn cells(node: &Value) -> Vec<Value> {
    let roles: Vec<String> = node.labels().keys().filter_map(|k| k.strip_prefix("node-role.kubernetes.io/").map(String::from)).collect();
    let info = |k: &str| {
        let v = str_at(node, &format!("/status/nodeInfo/{k}"));
        if v.is_empty() {
            "<unknown>".to_string()
        } else {
            v.to_string()
        }
    };
    vec![
        json!(node_status(node)),
        json!(if roles.is_empty() { "<none>".to_string() } else { roles.join(",") }),
        age_cell(node),
        json!(str_at(node, "/status/nodeInfo/kubeletVersion")),
        json!(address(node, "InternalIP")),
        json!(address(node, "ExternalIP")),
        json!(info("osImage")),
        json!(info("kernelVersion")),
        json!(info("containerRuntimeVersion")),
    ]
}
