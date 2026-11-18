//! `NamespaceLifecycle` admission plugin.
//!
//! * Rejects creating objects in a namespace that does not exist or is terminating.
//! * Forbids deleting the immortal namespaces (`default`, `kube-system`, `kube-public`).

use async_trait::async_trait;
use k8s_apimachinery::errors::ApiError;
use k8s_apimachinery::unstructured::ObjectExt;
use k8s_apiserver::admission::{AdmissionPlugin, Attributes, Operation};
use k8s_apiserver::StrategyContext;
use serde_json::Value;

pub const PLUGIN_NAME: &str = "NamespaceLifecycle";

pub const IMMORTAL_NAMESPACES: &[&str] = &["default", "kube-system", "kube-public"];

pub struct NamespaceLifecycle;

#[async_trait]
impl AdmissionPlugin for NamespaceLifecycle {
    fn name(&self) -> &str {
        PLUGIN_NAME
    }

    async fn validate(&self, attrs: &Attributes<'_>, _obj: &Value, ctx: &StrategyContext<'_>) -> Result<(), ApiError> {
        let r = attrs.resource;
        if r.group.is_empty() && r.plural == "namespaces" {
            if attrs.operation == Operation::Delete && IMMORTAL_NAMESPACES.contains(&attrs.name) {
                return Err(ApiError::forbidden(format!("namespaces \"{}\" is forbidden: this namespace may not be deleted", attrs.name)));
            }
            return Ok(());
        }
        if !r.namespaced || attrs.operation != Operation::Create {
            return Ok(());
        }
        let ns_key = format!("/registry/namespaces/{}", attrs.namespace);
        let gr = r.group_resource();
        match ctx.storage.get(&ns_key).await {
            Ok(ns) => {
                let terminating =
                    ns.deletion_timestamp().is_some() || ns.pointer("/status/phase").and_then(Value::as_str) == Some("Terminating");
                if terminating {
                    return Err(ApiError::forbidden(format!(
                        "{gr} \"{}\" is forbidden: unable to create new content in namespace {} because it is being terminated",
                        attrs.name, attrs.namespace
                    )));
                }
                Ok(())
            }
            Err(_) => Err(ApiError::not_found("", "namespaces", attrs.namespace)),
        }
    }
}
