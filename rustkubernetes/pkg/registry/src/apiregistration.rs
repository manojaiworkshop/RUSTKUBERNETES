//! Port of `kube-aggregator/pkg/registry/apiservice`: `APIService`
//! (`apiregistration.k8s.io/v1`), which tells the aggregator which group/version
//! a local or extension API server serves.

use k8s_apimachinery::validation::{ErrorList, FieldError};
use k8s_apiserver::table::{age_cell, Column};
use serde_json::{json, Value};

use crate::generic::{str_at, SimpleStrategy};

pub fn apiservice() -> SimpleStrategy {
    SimpleStrategy::new(true).name_validation(validate_name).defaults(defaults).validate(validate).table(columns, cells)
}

/// `ValidateAPIServiceName`: `<version>.<group>`; the group may be empty (`v1.` is the core group).
fn validate_name(name: &str) -> Vec<String> {
    let Some((version, group)) = name.split_once('.') else {
        return vec!["must be in the form: <version>.<group>".into()];
    };
    let mut errs = k8s_apimachinery::validation::is_dns1123_label(version);
    if !group.is_empty() {
        errs.extend(k8s_apimachinery::validation::is_dns1123_subdomain(group));
    }
    errs
}

/// `SetDefaults_ServiceReference`: port 443.
fn defaults(obj: &mut Value) {
    if let Some(svc) = obj.pointer_mut("/spec/service").and_then(Value::as_object_mut) {
        svc.entry("port").or_insert(json!(443));
    }
}

/// `ValidateAPIService` (main rules).
fn validate(obj: &Value) -> ErrorList {
    let mut errs = vec![];
    let group = str_at(obj, "/spec/group");
    let version = str_at(obj, "/spec/version");
    let name = str_at(obj, "/metadata/name");
    if version.is_empty() {
        errs.push(FieldError::required("spec.version", ""));
    }
    if name != format!("{version}.{group}") {
        errs.push(FieldError::invalid("metadata.name", name, format!("must be `spec.version+\".\"+spec.group`: {version:?}")));
    }
    let gpm = obj.pointer("/spec/groupPriorityMinimum").and_then(Value::as_i64).unwrap_or(0);
    if !(1..=20000).contains(&gpm) {
        errs.push(FieldError::invalid("spec.groupPriorityMinimum", gpm, "must be positive and less than 20000"));
    }
    let vp = obj.pointer("/spec/versionPriority").and_then(Value::as_i64).unwrap_or(0);
    if !(1..=1000).contains(&vp) {
        errs.push(FieldError::invalid("spec.versionPriority", vp, "must be positive and less than 1000"));
    }
    let has_service = obj.pointer("/spec/service").is_some_and(|s| s.is_object());
    if !has_service {
        if !str_at(obj, "/spec/caBundle").is_empty() {
            errs.push(FieldError::invalid("spec.caBundle", "...", "local APIServices may not have a caBundle"));
        }
        if obj.pointer("/spec/insecureSkipTLSVerify").and_then(Value::as_bool) == Some(true) {
            errs.push(FieldError::invalid("spec.insecureSkipTLSVerify", true, "local APIServices may not have insecureSkipTLSVerify"));
        }
    } else {
        if str_at(obj, "/spec/service/namespace").is_empty() {
            errs.push(FieldError::required("spec.service.namespace", ""));
        }
        if str_at(obj, "/spec/service/name").is_empty() {
            errs.push(FieldError::required("spec.service.name", ""));
        }
        let insecure = obj.pointer("/spec/insecureSkipTLSVerify").and_then(Value::as_bool) == Some(true);
        if insecure && !str_at(obj, "/spec/caBundle").is_empty() {
            errs.push(FieldError::invalid("spec.insecureSkipTLSVerify", true, "may not be true if caBundle is present"));
        }
    }
    errs
}

fn columns() -> Vec<Column> {
    vec![
        Column::new("Service", "string", "The reference to the service that hosts this API endpoint."),
        Column::new("Available", "string", "Whether this service is available."),
        Column::new("Age", "string", ""),
    ]
}

/// Printer: `Local` or `<namespace>/<name>`; `True`, or `False (<reason>)`.
fn cells(obj: &Value) -> Vec<Value> {
    let service = match obj.pointer("/spec/service").filter(|s| s.is_object()) {
        Some(s) => format!("{}/{}", str_at(s, "/namespace"), str_at(s, "/name")),
        None => "Local".into(),
    };
    let cond = obj.pointer("/status/conditions").and_then(Value::as_array).and_then(|c| c.iter().find(|c| c["type"] == "Available"));
    let available = match cond {
        None => "Unknown".to_string(),
        Some(c) => {
            let status = c["status"].as_str().unwrap_or("Unknown");
            let reason = c["reason"].as_str().unwrap_or("");
            if status == "True" || reason.is_empty() {
                status.to_string()
            } else {
                format!("{status} ({reason})")
            }
        }
    };
    vec![json!(service), json!(available), age_cell(obj)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printer_and_validation() {
        let mut a = json!({"metadata": {"name": "v1beta1.metrics.k8s.io"}, "spec": {"group": "metrics.k8s.io", "version": "v1beta1",
            "groupPriorityMinimum": 100, "versionPriority": 100, "insecureSkipTLSVerify": true,
            "service": {"namespace": "kube-system", "name": "metrics-server"}},
            "status": {"conditions": [{"type": "Available", "status": "False", "reason": "MissingEndpoints"}]}});
        defaults(&mut a);
        assert_eq!(a["spec"]["service"]["port"], 443);
        assert!(validate(&a).is_empty());
        assert_eq!(cells(&a)[..2], [json!("kube-system/metrics-server"), json!("False (MissingEndpoints)")]);
        let local = json!({"metadata": {"name": "v1.apps"}, "spec": {"group": "apps", "version": "v1", "groupPriorityMinimum": 17800, "versionPriority": 15},
            "status": {"conditions": [{"type": "Available", "status": "True", "reason": "Local"}]}});
        assert_eq!(cells(&local)[..2], [json!("Local"), json!("True")]);
        let bad =
            json!({"metadata": {"name": "x"}, "spec": {"group": "g", "version": "v1", "groupPriorityMinimum": 0, "versionPriority": 1}});
        assert_eq!(validate(&bad).len(), 2);
        assert!(validate_name("v1.").is_empty() && validate_name("v1beta1.metrics.k8s.io").is_empty());
        assert!(!validate_name("v1").is_empty());
    }
}
