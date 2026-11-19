//! `DefaultStorageClass` (`plugin/pkg/admission/storage/storageclass/setdefault`):
//! a new claim without a class gets the default StorageClass.

use async_trait::async_trait;
use k8s_apimachinery::errors::ApiError;
use k8s_apiserver::admission::{AdmissionPlugin, Attributes, Operation};
use k8s_apiserver::registry::StrategyContext;
use serde_json::{json, Value};

pub const PLUGIN_NAME: &str = "DefaultStorageClass";
const DEFAULT_ANNOTATIONS: [&str; 2] = ["storageclass.kubernetes.io/is-default-class", "storageclass.beta.kubernetes.io/is-default-class"];

/// `GetDefaultClass`: of the classes marked default, the newest (then by name).
pub fn default_class(classes: &[Value]) -> Option<String> {
    let mut defaults: Vec<&Value> = classes
        .iter()
        .filter(|c| {
            DEFAULT_ANNOTATIONS
                .iter()
                .any(|a| c.pointer(&format!("/metadata/annotations/{}", a.replace('/', "~1"))).and_then(Value::as_str) == Some("true"))
        })
        .collect();
    let ts = |c: &Value| c.pointer("/metadata/creationTimestamp").and_then(Value::as_str).unwrap_or("").to_string();
    let name = |c: &Value| c.pointer("/metadata/name").and_then(Value::as_str).unwrap_or("").to_string();
    defaults.sort_by(|a, b| ts(b).cmp(&ts(a)).then_with(|| name(a).cmp(&name(b))));
    defaults.first().map(|c| name(c))
}

pub struct DefaultStorageClass;

#[async_trait]
impl AdmissionPlugin for DefaultStorageClass {
    fn name(&self) -> &str {
        PLUGIN_NAME
    }

    async fn admit(&self, a: &Attributes<'_>, obj: &mut Value, ctx: &StrategyContext<'_>) -> Result<(), ApiError> {
        let r = a.resource;
        if a.operation != Operation::Create || a.subresource.is_some() || !r.group.is_empty() || r.plural != "persistentvolumeclaims" {
            return Ok(());
        }
        // `PersistentVolumeClaimHasClass`: an explicit class (even "") or the beta annotation.
        let has_class = obj.pointer("/spec/storageClassName").is_some_and(|c| !c.is_null())
            || obj.pointer("/metadata/annotations/volume.beta.kubernetes.io~1storage-class").is_some();
        if has_class {
            return Ok(());
        }
        let Some(info) = ctx.registry.lookup("storage.k8s.io", "v1", "storageclasses") else { return Ok(()) };
        let classes = ctx.storage.list(&info.prefix("")).await.map(|(items, _)| items).unwrap_or_default();
        if let Some(def) = default_class(&classes) {
            if let Some(spec) = obj.get_mut("spec").and_then(Value::as_object_mut) {
                spec.insert("storageClassName".into(), json!(def));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_default_wins() {
        let c = |n: &str, ts: &str, d: bool| {
            json!({"metadata": {"name": n, "creationTimestamp": ts,
                "annotations": if d { json!({"storageclass.kubernetes.io/is-default-class": "true"}) } else { json!({}) }}})
        };
        let list =
            vec![c("old", "2026-01-01T00:00:00Z", true), c("new", "2026-02-01T00:00:00Z", true), c("plain", "2026-03-01T00:00:00Z", false)];
        assert_eq!(default_class(&list).as_deref(), Some("new"));
        assert_eq!(default_class(&list[2..]), None);
    }
}
