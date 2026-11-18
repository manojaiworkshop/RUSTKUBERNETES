//! Minimal OpenAPI v3 publishing (`k8s.io/kube-openapi` handler3).
//!
//! `kubectl create/apply` ask `/openapi/v3` whether the server supports the
//! `fieldValidation` query parameter for a kind (by looking for a PATCH
//! operation tagged with `x-kubernetes-group-version-kind`). When it does,
//! kubectl relies on server-side validation instead of client-side schemas.
//!
//! Phase 1 publishes the paths and operations (no schemas yet).

use serde_json::{json, Map, Value};

use crate::registry::Registry;

fn gv_path(group: &str, version: &str) -> String {
    if group.is_empty() {
        format!("api/{version}")
    } else {
        format!("apis/{group}/{version}")
    }
}

/// `GET /openapi/v3`: index of group-version documents.
pub fn root(registry: &Registry, hash: &str) -> Value {
    let mut paths = Map::new();
    for (group, versions) in registry.groups() {
        for v in versions {
            let p = gv_path(&group, &v);
            paths.insert(p.clone(), json!({"serverRelativeURL": format!("/openapi/v3/{p}?hash={hash}")}));
        }
    }
    json!({"paths": paths})
}

fn param(name: &str, in_: &str) -> Value {
    json!({"name": name, "in": in_, "schema": {"type": "string", "uniqueItems": true}})
}

/// `GET /openapi/v3/api/v1`, `/openapi/v3/apis/<g>/<v>`.
pub fn group_version(registry: &Registry, path: &str, version_info: &str) -> Option<Value> {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let (group, version) = match parts.as_slice() {
        ["api", v] => ("", *v),
        ["apis", g, v] => (*g, *v),
        _ => return None,
    };
    let resources = registry.group_version(group, version);
    if resources.is_empty() {
        return None;
    }
    let base = format!("/{}", gv_path(group, version));
    let mut paths = Map::new();
    for r in resources {
        let gvk = json!({"group": r.group, "version": r.version, "kind": r.kind});
        let collection =
            if r.namespaced { format!("{base}/namespaces/{{namespace}}/{}", r.plural) } else { format!("{base}/{}", r.plural) };
        let item = format!("{collection}/{{name}}");
        let common = vec![param("dryRun", "query"), param("fieldManager", "query"), param("fieldValidation", "query")];
        let op = |id: &str, action: &str| {
            json!({
                "operationId": format!("{id}{}", r.kind),
                "x-kubernetes-action": action,
                "x-kubernetes-group-version-kind": gvk,
                "parameters": common,
                "responses": {"200": {"description": "OK"}},
            })
        };
        paths.insert(collection, json!({"post": op("create", "post"), "get": op("list", "list")}));
        paths.insert(
            item,
            json!({"get": op("read", "get"), "put": op("replace", "put"),
                   "patch": op("patch", "patch"), "delete": op("delete", "delete")}),
        );
    }
    Some(json!({
        "openapi": "3.0.0",
        "info": {"title": "Kubernetes", "version": version_info},
        "paths": paths,
        "components": {"schemas": {}},
    }))
}
