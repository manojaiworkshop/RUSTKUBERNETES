//! Helpers for working with objects as untyped JSON (`unstructured.Unstructured`).
//!
//! The API server stores every object as a `serde_json::Value`, exactly like
//! the Go API server treats CRDs. Typed structs (in `k8s-api`) are used for
//! validation and by controllers.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// Reads a dotted path (`spec.template.metadata.labels`).
pub fn get_path<'a>(obj: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = obj;
    for part in path.split('.') {
        cur = cur.get(part)?;
    }
    Some(cur)
}

/// Reads a dotted path as a string (numbers and bools are stringified).
pub fn get_path_str(obj: &Value, path: &str) -> Option<String> {
    match get_path(obj, path)? {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        v @ (Value::Number(_) | Value::Bool(_)) => Some(v.to_string()),
        _ => None,
    }
}

/// Sets a dotted path, creating intermediate objects.
pub fn set_path(obj: &mut Value, path: &str, value: Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut cur = obj;
    for part in &parts[..parts.len() - 1] {
        if !cur.get(*part).is_some_and(Value::is_object) {
            cur[*part] = Value::Object(Map::new());
        }
        cur = cur.get_mut(*part).unwrap();
    }
    cur[parts[parts.len() - 1]] = value;
}

/// Removes a dotted path, returning the old value.
pub fn remove_path(obj: &mut Value, path: &str) -> Option<Value> {
    let (parent, last) = match path.rsplit_once('.') {
        Some((p, l)) => (get_path_mut(obj, p)?, l),
        None => (obj, path),
    };
    parent.as_object_mut()?.remove(last)
}

pub fn get_path_mut<'a>(obj: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    let mut cur = obj;
    for part in path.split('.') {
        cur = cur.get_mut(part)?;
    }
    Some(cur)
}

/// Convenience accessors on an object's metadata.
pub trait ObjectExt {
    fn name(&self) -> &str;
    fn namespace(&self) -> &str;
    fn uid(&self) -> &str;
    fn kind(&self) -> &str;
    fn api_version(&self) -> &str;
    fn resource_version(&self) -> &str;
    fn labels(&self) -> BTreeMap<String, String>;
    fn creation_timestamp(&self) -> &str;
    fn deletion_timestamp(&self) -> Option<&str>;
    fn finalizers(&self) -> Vec<String>;
    fn metadata_mut(&mut self) -> &mut Map<String, Value>;
}

impl ObjectExt for Value {
    fn name(&self) -> &str {
        self.pointer("/metadata/name").and_then(Value::as_str).unwrap_or("")
    }
    fn namespace(&self) -> &str {
        self.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or("")
    }
    fn uid(&self) -> &str {
        self.pointer("/metadata/uid").and_then(Value::as_str).unwrap_or("")
    }
    fn kind(&self) -> &str {
        self.get("kind").and_then(Value::as_str).unwrap_or("")
    }
    fn api_version(&self) -> &str {
        self.get("apiVersion").and_then(Value::as_str).unwrap_or("")
    }
    fn resource_version(&self) -> &str {
        self.pointer("/metadata/resourceVersion").and_then(Value::as_str).unwrap_or("")
    }
    fn labels(&self) -> BTreeMap<String, String> {
        self.pointer("/metadata/labels")
            .and_then(Value::as_object)
            .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
            .unwrap_or_default()
    }
    fn creation_timestamp(&self) -> &str {
        self.pointer("/metadata/creationTimestamp").and_then(Value::as_str).unwrap_or("")
    }
    fn deletion_timestamp(&self) -> Option<&str> {
        self.pointer("/metadata/deletionTimestamp").and_then(Value::as_str)
    }
    fn finalizers(&self) -> Vec<String> {
        self.pointer("/metadata/finalizers")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    }
    fn metadata_mut(&mut self) -> &mut Map<String, Value> {
        if !self.get("metadata").is_some_and(Value::is_object) {
            self["metadata"] = Value::Object(Map::new());
        }
        self["metadata"].as_object_mut().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn paths() {
        let mut v = json!({"a": {"b": 1}});
        assert_eq!(get_path_str(&v, "a.b").as_deref(), Some("1"));
        set_path(&mut v, "x.y.z", json!("q"));
        assert_eq!(get_path_str(&v, "x.y.z").as_deref(), Some("q"));
        assert_eq!(remove_path(&mut v, "x.y.z"), Some(json!("q")));
        assert!(get_path(&v, "x.y.z").is_none());
    }
}
