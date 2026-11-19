//! Port of `pkg/registry/core/node`.

use k8s_apimachinery::net::{is_dual_stack, IpNet};
use k8s_apimachinery::unstructured::ObjectExt;
use k8s_apimachinery::validation::{ErrorList, FieldError};
use k8s_apiserver::table::{age_cell, Column};
use serde_json::{json, Value};

use crate::generic::{str_at, SimpleStrategy};

pub fn strategy() -> SimpleStrategy {
    SimpleStrategy::new(true)
        .status_updates_metadata()
        .defaults(defaults)
        .validate(validate)
        .validate_update(validate_update)
        .table(columns, cells)
}

/// The v1 conversion keeps the legacy `podCIDR` equal to `podCIDRs[0]`
/// (`Convert_v1_NodeSpec_To_core_NodeSpec` and back).
fn defaults(node: &mut Value) {
    let Some(spec) = node.get_mut("spec").and_then(Value::as_object_mut) else { return };
    let list: Vec<Value> = spec.get("podCIDRs").and_then(Value::as_array).cloned().unwrap_or_default();
    let single = spec.get("podCIDR").and_then(Value::as_str).unwrap_or("").to_string();
    if let Some(first) = list.first() {
        spec.insert("podCIDR".into(), first.clone());
    } else if !single.is_empty() {
        spec.insert("podCIDRs".into(), json!([single]));
    }
}

fn pod_cidrs(node: &Value) -> Vec<String> {
    node.pointer("/spec/podCIDRs")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|v| v.as_str().unwrap_or("").to_string()).collect())
        .unwrap_or_default()
}

/// `ValidateNode` (pod CIDR part).
fn validate(node: &Value) -> ErrorList {
    let mut errs = vec![];
    let cidrs = pod_cidrs(node);
    let mut parsed = vec![];
    for (i, c) in cidrs.iter().enumerate() {
        match c.parse::<IpNet>() {
            Ok(n) => parsed.push(n),
            Err(_) => errs.push(FieldError::invalid(
                format!("spec.podCIDRs[{i}]"),
                c,
                "must be a valid CIDR value, (e.g. 10.9.8.0/24 or 2001:db8::/64)",
            )),
        }
    }
    if cidrs.len() > 1 && (cidrs.len() > 2 || parsed.len() == cidrs.len() && !is_dual_stack(&parsed)) {
        errs.push(FieldError::invalid("spec.podCIDRs", format!("{cidrs:?}"), "may specify no more than one CIDR for each IP family"));
    }
    errs
}

/// `ValidateNodeUpdate`: the controller manager may assign pod CIDRs once.
fn validate_update(new: &Value, old: &Value) -> ErrorList {
    let mut errs = validate(new);
    let old_cidrs = pod_cidrs(old);
    if !old_cidrs.is_empty() && old_cidrs != pod_cidrs(new) {
        errs.push(FieldError::forbidden("spec.podCIDRs", "node updates may not change podCIDR except from \"\" to valid"));
    }
    errs
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pod_cidr_rules() {
        let mut n = json!({"spec": {"podCIDRs": ["10.244.1.0/24"]}});
        defaults(&mut n);
        assert_eq!(n["spec"]["podCIDR"], "10.244.1.0/24");
        let mut legacy = json!({"spec": {"podCIDR": "10.244.2.0/24"}});
        defaults(&mut legacy);
        assert_eq!(legacy["spec"]["podCIDRs"], json!(["10.244.2.0/24"]));
        assert!(validate(&n).is_empty());
        assert_eq!(validate(&json!({"spec": {"podCIDRs": ["nope"]}})).len(), 1);
        assert_eq!(validate(&json!({"spec": {"podCIDRs": ["10.1.0.0/24", "10.2.0.0/24"]}})).len(), 1);
        assert!(validate(&json!({"spec": {"podCIDRs": ["10.1.0.0/24", "fd00::/64"]}})).is_empty());
        // "" -> value is allowed, value -> other value is not
        assert!(validate_update(&n, &json!({"spec": {}})).is_empty());
        let moved = json!({"spec": {"podCIDRs": ["10.244.9.0/24"]}});
        assert!(validate_update(&moved, &n)[0].message().contains("may not change podCIDR"));
    }
}
