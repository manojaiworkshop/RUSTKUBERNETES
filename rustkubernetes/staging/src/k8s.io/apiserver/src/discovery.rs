//! Port of `k8s.io/apiserver/pkg/endpoints/discovery`.
//!
//! Serves the legacy (unaggregated) discovery documents that `kubectl` and
//! client libraries use to map kinds to resources:
//! `/api` (APIVersions), `/apis` (APIGroupList), `/apis/<g>` (APIGroup) and
//! `/api/v1`, `/apis/<g>/<v>` (APIResourceList).

use serde_json::{json, Value};

use crate::registry::Registry;

pub const VERBS: &[&str] = &["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"];

pub fn api_versions(server_address: &str) -> Value {
    json!({
        "kind": "APIVersions",
        "versions": ["v1"],
        "serverAddressByClientCIDRs": [{"clientCIDR": "0.0.0.0/0", "serverAddress": server_address}],
    })
}

fn group_json(name: &str, versions: &[String]) -> Value {
    let vs: Vec<Value> = versions.iter().map(|v| json!({"groupVersion": format!("{name}/{v}"), "version": v})).collect();
    json!({
        "name": name,
        "versions": vs,
        "preferredVersion": vs.first().cloned().unwrap_or(Value::Null),
    })
}

pub fn api_group_list(registry: &Registry) -> Value {
    let groups: Vec<Value> = registry.groups().iter().filter(|(g, _)| !g.is_empty()).map(|(g, vs)| group_json(g, vs)).collect();
    json!({"kind": "APIGroupList", "apiVersion": "v1", "groups": groups})
}

pub fn api_group(registry: &Registry, group: &str) -> Option<Value> {
    let groups = registry.groups();
    let versions = groups.get(group).filter(|_| !group.is_empty())?;
    let mut g = group_json(group, versions);
    g["kind"] = json!("APIGroup");
    g["apiVersion"] = json!("v1");
    Some(g)
}

pub fn api_resource_list(registry: &Registry, group: &str, version: &str) -> Option<Value> {
    let resources = registry.group_version(group, version);
    if resources.is_empty() {
        return None;
    }
    let mut list = vec![];
    for r in &resources {
        let mut entry = json!({
            "name": r.plural,
            "singularName": r.singular,
            "namespaced": r.namespaced,
            "kind": r.kind,
            "verbs": if r.strategy.is_virtual() { json!(["create"]) } else { json!(VERBS) },
        });
        if !r.short_names.is_empty() {
            entry["shortNames"] = json!(r.short_names);
        }
        if !r.categories.is_empty() {
            entry["categories"] = json!(r.categories);
        }
        list.push(entry);
        for sub in &r.subresources {
            let (kind, group, version, verbs): (&str, &str, &str, Vec<&str>) = match sub.as_str() {
                "status" => (&r.kind, "", "", vec!["get", "patch", "update"]),
                "scale" => ("Scale", "autoscaling", "v1", vec!["get", "patch", "update"]),
                "binding" => ("Binding", "", "", vec!["create"]),
                "eviction" => ("Eviction", "policy", "v1", vec!["create"]),
                "token" => ("TokenRequest", "authentication.k8s.io", "v1", vec!["create"]),
                "exec" => ("PodExecOptions", "", "", vec!["create", "get"]),
                "attach" => ("PodAttachOptions", "", "", vec!["create", "get"]),
                "portforward" => ("PodPortForwardOptions", "", "", vec!["create", "get"]),
                "log" => ("Pod", "", "", vec!["get"]),
                _ => (&r.kind, "", "", vec!["get"]),
            };
            let mut e = json!({
                "name": format!("{}/{}", r.plural, sub),
                "singularName": "",
                "namespaced": r.namespaced,
                "kind": kind,
                "verbs": verbs,
            });
            if !group.is_empty() {
                e["group"] = json!(group);
                e["version"] = json!(version);
            }
            list.push(e);
        }
    }
    let gv = if group.is_empty() { version.to_string() } else { format!("{group}/{version}") };
    Some(json!({"kind": "APIResourceList", "apiVersion": "v1", "groupVersion": gv, "resources": list}))
}
