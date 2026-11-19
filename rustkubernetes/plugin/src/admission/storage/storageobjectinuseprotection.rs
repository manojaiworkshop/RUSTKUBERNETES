//! `StorageObjectInUseProtection` (`plugin/pkg/admission/storage/storageobjectinuseprotection`):
//! new claims and volumes get the protection finalizers, so they are not removed
//! while a pod (or a claim) still uses them.

use async_trait::async_trait;
use k8s_apimachinery::errors::ApiError;
use k8s_apiserver::admission::{AdmissionPlugin, Attributes, Operation};
use k8s_apiserver::registry::StrategyContext;
use serde_json::{json, Value};

pub const PLUGIN_NAME: &str = "StorageObjectInUseProtection";
pub const PVC_PROTECTION_FINALIZER: &str = "kubernetes.io/pvc-protection";
pub const PV_PROTECTION_FINALIZER: &str = "kubernetes.io/pv-protection";

pub struct StorageObjectInUseProtection;

#[async_trait]
impl AdmissionPlugin for StorageObjectInUseProtection {
    fn name(&self) -> &str {
        PLUGIN_NAME
    }

    async fn admit(&self, a: &Attributes<'_>, obj: &mut Value, _ctx: &StrategyContext<'_>) -> Result<(), ApiError> {
        let r = a.resource;
        if a.operation != Operation::Create || a.subresource.is_some() || !r.group.is_empty() {
            return Ok(());
        }
        let finalizer = match r.plural.as_str() {
            "persistentvolumeclaims" => PVC_PROTECTION_FINALIZER,
            "persistentvolumes" => PV_PROTECTION_FINALIZER,
            _ => return Ok(()),
        };
        let Some(meta) = obj.get_mut("metadata").and_then(Value::as_object_mut) else { return Ok(()) };
        let list = meta.entry("finalizers").or_insert(json!([]));
        if let Some(arr) = list.as_array_mut() {
            if !arr.iter().any(|f| f == finalizer) {
                arr.push(json!(finalizer));
            }
        }
        Ok(())
    }
}
