//! Port of `pkg/registry/core/namespace`.
//!
//! Deleting a namespace only marks it `Terminating` while `spec.finalizers`
//! contains `kubernetes`. The namespace controller (kube-controller-manager)
//! deletes all content and then removes the finalizer through the
//! `namespaces/<name>/finalize` subresource, after which the API server
//! removes the namespace.

use async_trait::async_trait;
use k8s_apimachinery::validation::ErrorList;
use k8s_apiserver::table::{age_cell, Column};
use k8s_apiserver::Strategy;
use kube_apis::core::{defaults, validation};
use serde_json::{json, Value};

use crate::generic::str_at;

pub struct NamespaceStrategy;

#[async_trait]
impl Strategy for NamespaceStrategy {
    fn prepare_for_create(&self, obj: &mut Value) {
        defaults::set_namespace_defaults(obj);
    }

    fn prepare_for_update(&self, new: &mut Value, old: &Value) {
        // spec.finalizers can only be changed through /finalize.
        if let Some(f) = old.pointer("/spec/finalizers") {
            new["spec"]["finalizers"] = f.clone();
        }
        defaults::set_namespace_defaults(new);
    }

    fn validate(&self, obj: &Value) -> ErrorList {
        validation::validate_namespace(obj)
    }

    fn can_remove(&self, obj: &Value) -> bool {
        obj.pointer("/spec/finalizers").and_then(Value::as_array).is_none_or(|f| f.is_empty())
    }

    fn on_delete_marked(&self, obj: &mut Value) {
        obj["status"]["phase"] = json!("Terminating");
    }

    fn table_columns(&self) -> Vec<Column> {
        vec![Column::new("Status", "string", "The status of the namespace"), Column::new("Age", "string", "")]
    }

    fn table_cells(&self, obj: &Value) -> Vec<Value> {
        let phase = str_at(obj, "/status/phase");
        vec![json!(if phase.is_empty() { "Active" } else { phase }), age_cell(obj)]
    }
}
