//! Patch application: JSON Patch (RFC 6902), JSON Merge Patch (RFC 7386) and a
//! simplified Strategic Merge Patch.
//!
//! Strategic merge patch in Go uses struct tags (`patchMergeKey`,
//! `patchStrategy`) from the typed API. Here we use a table of well-known list
//! fields and their merge keys, which covers what `kubectl apply/patch/edit`
//! generates for built-in types. Lists not in the table are replaced, like
//! with a merge patch.

use k8s_apimachinery::errors::ApiError;
use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchType {
    Json,
    Merge,
    Strategic,
    Apply,
}

impl PatchType {
    pub fn from_content_type(ct: &str) -> Option<Self> {
        let ct = ct.split(';').next().unwrap_or("").trim();
        match ct {
            "application/json-patch+json" => Some(PatchType::Json),
            "application/merge-patch+json" => Some(PatchType::Merge),
            "application/strategic-merge-patch+json" => Some(PatchType::Strategic),
            "application/apply-patch+yaml" | "application/apply-patch+json" => Some(PatchType::Apply),
            _ => None,
        }
    }
}

/// Applies `patch` (raw body) to `target`.
pub fn apply_patch(target: &Value, patch_type: PatchType, body: &[u8]) -> Result<Value, ApiError> {
    let mut out = target.clone();
    match patch_type {
        PatchType::Json => {
            let p: json_patch::Patch =
                serde_json::from_slice(body).map_err(|e| ApiError::bad_request(format!("invalid JSON patch: {e}")))?;
            json_patch::patch(&mut out, &p).map_err(|e| {
                ApiError::new(422, "Invalid", format!("the server rejected our request due to an error in our request: {e}"))
            })?;
        }
        PatchType::Merge => {
            let p: Value = serde_json::from_slice(body).map_err(|e| ApiError::bad_request(format!("invalid merge patch: {e}")))?;
            json_patch::merge(&mut out, &p);
        }
        PatchType::Strategic => {
            let p: Value =
                serde_json::from_slice(body).map_err(|e| ApiError::bad_request(format!("invalid strategic merge patch: {e}")))?;
            strategic_merge(&mut out, &p, "");
        }
        PatchType::Apply => {
            let p: Value = serde_yaml::from_slice(body).map_err(|e| ApiError::bad_request(format!("invalid apply patch: {e}")))?;
            strategic_merge(&mut out, &p, "");
        }
    }
    Ok(out)
}

/// Merge key for well-known list fields (by field name), if the list is mergeable.
fn merge_key(field: &str, items: &[Value]) -> Option<&'static str> {
    let first = items.iter().find(|v| v.is_object())?;
    let key = match field {
        "containers" | "initContainers" | "ephemeralContainers" | "volumes" | "env" | "imagePullSecrets" | "resourceClaims" => "name",
        "volumeMounts" => "mountPath",
        "volumeDevices" => "devicePath",
        "conditions" | "addresses" => "type",
        "taints" => "key",
        "hostAliases" => "ip",
        "ports" => {
            if first.get("containerPort").is_some() {
                "containerPort"
            } else if first.get("port").is_some() {
                "port"
            } else {
                return None;
            }
        }
        _ => return None,
    };
    Some(key)
}

/// Simplified strategic merge of `patch` into `target`.
pub fn strategic_merge(target: &mut Value, patch: &Value, field: &str) {
    let Value::Object(pmap) = patch else {
        *target = patch.clone();
        return;
    };
    if pmap.get("$patch").and_then(Value::as_str) == Some("replace") {
        *target = strip_directives(patch);
        return;
    }
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let _ = field;
    let tmap = target.as_object_mut().unwrap();
    for (k, pv) in pmap {
        if let Some(list_field) = k.strip_prefix("$deleteFromPrimitiveList/") {
            if let (Some(Value::Array(list)), Value::Array(del)) = (tmap.get_mut(list_field), pv) {
                list.retain(|v| !del.contains(v));
            }
            continue;
        }
        if k.starts_with('$') {
            // $setElementOrder/*, $retainKeys, $patch: ordering/retention hints we ignore.
            continue;
        }
        match pv {
            Value::Null => {
                tmap.remove(k);
            }
            Value::Array(plist) => {
                let existing = tmap.get(k).and_then(Value::as_array).cloned();
                match (existing, merge_key(k, plist)) {
                    (Some(mut tlist), Some(mk)) => {
                        merge_list(&mut tlist, plist, mk);
                        tmap.insert(k.clone(), Value::Array(tlist));
                    }
                    _ => {
                        let cleaned: Vec<Value> = plist
                            .iter()
                            .filter(|v| v.get("$patch").and_then(Value::as_str) != Some("delete"))
                            .map(strip_directives)
                            .collect();
                        tmap.insert(k.clone(), Value::Array(cleaned));
                    }
                }
            }
            Value::Object(_) => {
                if pv.get("$patch").and_then(Value::as_str) == Some("delete") {
                    tmap.remove(k);
                    continue;
                }
                let entry = tmap.entry(k.clone()).or_insert_with(|| Value::Object(Map::new()));
                strategic_merge(entry, pv, k);
            }
            other => {
                tmap.insert(k.clone(), other.clone());
            }
        }
    }
}

fn merge_list(target: &mut Vec<Value>, patch: &[Value], key: &str) {
    for item in patch {
        let Some(kv) = item.get(key) else {
            target.push(strip_directives(item));
            continue;
        };
        let pos = target.iter().position(|t| t.get(key) == Some(kv));
        let delete = item.get("$patch").and_then(Value::as_str) == Some("delete");
        match (pos, delete) {
            (Some(i), true) => {
                target.remove(i);
            }
            (None, true) => {}
            (Some(i), false) => strategic_merge(&mut target[i], item, ""),
            (None, false) => target.push(strip_directives(item)),
        }
    }
}

fn strip_directives(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            Value::Object(m.iter().filter(|(k, _)| !k.starts_with('$')).map(|(k, v)| (k.clone(), strip_directives(v))).collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(strip_directives).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strategic_merges_containers_by_name() {
        let mut pod = json!({"spec": {"containers": [
            {"name": "a", "image": "a:1"}, {"name": "b", "image": "b:1"}]}});
        let patch = json!({"spec": {
            "$setElementOrder/containers": [{"name": "a"}, {"name": "b"}],
            "containers": [{"name": "b", "image": "b:2"}]}});
        strategic_merge(&mut pod, &patch, "");
        assert_eq!(pod["spec"]["containers"][0]["image"], "a:1");
        assert_eq!(pod["spec"]["containers"][1]["image"], "b:2");
        assert!(pod["spec"].get("$setElementOrder/containers").is_none());
    }

    #[test]
    fn strategic_delete_and_null() {
        let mut obj = json!({"metadata": {"labels": {"a": "1", "b": "2"}},
            "spec": {"containers": [{"name": "a"}, {"name": "b"}]}});
        let patch = json!({"metadata": {"labels": {"a": null}},
            "spec": {"containers": [{"name": "a", "$patch": "delete"}]}});
        strategic_merge(&mut obj, &patch, "");
        assert_eq!(obj["metadata"]["labels"], json!({"b": "2"}));
        assert_eq!(obj["spec"]["containers"], json!([{"name": "b"}]));
    }

    #[test]
    fn json_and_merge_patch() {
        let base = json!({"spec": {"replicas": 1, "x": [1, 2]}});
        let out = apply_patch(&base, PatchType::Merge, br#"{"spec":{"replicas":3,"x":[9]}}"#).unwrap();
        assert_eq!(out, json!({"spec": {"replicas": 3, "x": [9]}}));
        let out = apply_patch(&base, PatchType::Json, br#"[{"op":"replace","path":"/spec/replicas","value":5}]"#).unwrap();
        assert_eq!(out["spec"]["replicas"], 5);
        assert!(apply_patch(&base, PatchType::Json, br#"[{"op":"remove","path":"/nope"}]"#).is_err());
    }
}
