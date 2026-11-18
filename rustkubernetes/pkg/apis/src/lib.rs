//! Port of `k8s.io/kubernetes/pkg/apis`.
//!
//! In Go every group has `types.go` (internal types), `<version>/defaults.go`
//! and `validation/validation.go`. Here:
//!
//! * **defaults** operate on untyped JSON (`serde_json::Value`) so that fields
//!   the Rust structs do not model are never lost;
//! * **validation** decodes into the typed structs from `k8s-api` and returns a
//!   `field.ErrorList` equivalent.

pub mod apps;
pub mod batch;
pub mod core;

use serde_json::Value;

/// Sets `obj[key] = value` if the key is absent or null.
pub(crate) fn default_field(obj: &mut Value, key: &str, value: Value) {
    if let Some(m) = obj.as_object_mut() {
        if m.get(key).is_none_or(Value::is_null) {
            m.insert(key.to_string(), value);
        }
    }
}

/// Returns a mutable reference to `obj[key]`, creating an empty object if absent.
pub(crate) fn child<'a>(obj: &'a mut Value, key: &str) -> &'a mut Value {
    default_field(obj, key, Value::Object(Default::default()));
    obj.get_mut(key).unwrap()
}

/// Decodes an untyped object into a typed struct (lossy: unknown fields are ignored).
pub fn decode<T: serde::de::DeserializeOwned>(v: &Value) -> Result<T, String> {
    serde_json::from_value(v.clone()).map_err(|e| e.to_string())
}
