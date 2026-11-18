//! Port of `pkg/apis/apps/validation/validation.go` (commonly used subset).

use k8s_api::apps::v1::{DaemonSet, Deployment, ReplicaSet, StatefulSet};
use k8s_api::core::v1::PodTemplateSpec;
use k8s_api::IntOrString;
use k8s_apimachinery::labels::Selector;
use k8s_apimachinery::meta::LabelSelector;
use k8s_apimachinery::validation::{validate_labels, ErrorList, FieldError};
use serde_json::Value;

use crate::core::validation::validate_pod_spec;
use crate::decode;

/// Validates `spec.selector` + `spec.template` (shared by all workload controllers).
fn validate_selector_and_template(selector: Option<&LabelSelector>, template: Option<&PodTemplateSpec>, errs: &mut ErrorList) {
    let sel = match selector {
        None => {
            errs.push(FieldError::required("spec.selector", ""));
            None
        }
        Some(s) if s.match_labels.as_ref().is_none_or(|m| m.is_empty()) && s.match_expressions.as_ref().is_none_or(|e| e.is_empty()) => {
            errs.push(FieldError::invalid("spec.selector", "{}", "empty selector is invalid for deployment"));
            None
        }
        Some(s) => {
            if let Some(ml) = &s.match_labels {
                errs.extend(validate_labels(ml, "spec.selector.matchLabels"));
            }
            match Selector::from_label_selector(s) {
                Ok(sel) => Some(sel),
                Err(e) => {
                    errs.push(FieldError::invalid("spec.selector", "", e));
                    None
                }
            }
        }
    };
    let Some(t) = template else {
        errs.push(FieldError::required("spec.template", ""));
        return;
    };
    let labels = t.metadata.as_ref().map(|m| m.labels()).unwrap_or_default();
    errs.extend(validate_labels(&labels, "spec.template.metadata.labels"));
    if let Some(sel) = sel {
        if !sel.matches(&labels) {
            errs.push(FieldError::invalid(
                "spec.template.metadata.labels",
                format!("{labels:?}"),
                "`selector` does not match template `labels`",
            ));
        }
    }
    match &t.spec {
        None => errs.push(FieldError::required("spec.template.spec", "")),
        Some(spec) => {
            errs.extend(validate_pod_spec(spec, "spec.template.spec"));
            if let Some(rp) = &spec.restart_policy {
                if rp != "Always" {
                    errs.push(FieldError::not_supported("spec.template.spec.restartPolicy", rp, &["Always"]));
                }
            }
        }
    }
}

fn validate_replicas(r: Option<i32>, errs: &mut ErrorList) {
    if let Some(r) = r {
        if r < 0 {
            errs.push(FieldError::invalid("spec.replicas", r, "must be greater than or equal to 0"));
        }
    }
}

fn decode_or<T: serde::de::DeserializeOwned>(obj: &Value, errs: &mut ErrorList) -> Option<T> {
    decode(obj).map_err(|e| errs.push(FieldError::invalid("<object>", "", format!("unable to decode: {e}")))).ok()
}

pub fn validate_deployment(obj: &Value) -> ErrorList {
    let mut errs = vec![];
    let Some(d) = decode_or::<Deployment>(obj, &mut errs) else { return errs };
    let spec = d.spec.unwrap_or_default();
    validate_replicas(spec.replicas, &mut errs);
    validate_selector_and_template(spec.selector.as_ref(), Some(&spec.template), &mut errs);
    if let Some(st) = &spec.strategy {
        let t = st.type_.as_deref().unwrap_or("RollingUpdate");
        match t {
            "Recreate" => {
                if st.rolling_update.is_some() {
                    errs.push(FieldError::forbidden(
                        "spec.strategy.rollingUpdate",
                        "may not be specified when strategy `type` is 'Recreate'",
                    ));
                }
            }
            "RollingUpdate" => {
                if let Some(ru) = &st.rolling_update {
                    let zero = |v: &Option<IntOrString>| match v {
                        Some(IntOrString::Int(0)) => true,
                        Some(IntOrString::String(s)) => s == "0%",
                        _ => false,
                    };
                    if zero(&ru.max_surge) && zero(&ru.max_unavailable) {
                        errs.push(FieldError::invalid(
                            "spec.strategy.rollingUpdate.maxUnavailable",
                            0,
                            "may not be 0 when `maxSurge` is 0",
                        ));
                    }
                    for (name, v) in [("maxSurge", &ru.max_surge), ("maxUnavailable", &ru.max_unavailable)] {
                        if let Some(v) = v {
                            if let Err(e) = v.scaled_value(100, true) {
                                errs.push(FieldError::invalid(format!("spec.strategy.rollingUpdate.{name}"), v, e));
                            }
                        }
                    }
                }
            }
            other => errs.push(FieldError::not_supported("spec.strategy.type", other, &["Recreate", "RollingUpdate"])),
        }
    }
    if let Some(r) = spec.revision_history_limit {
        if r < 0 {
            errs.push(FieldError::invalid("spec.revisionHistoryLimit", r, "must be greater than or equal to 0"));
        }
    }
    errs
}

/// apps/v1 selectors are immutable.
fn validate_selector_immutable(new: &Value, old: &Value) -> ErrorList {
    if new.pointer("/spec/selector") != old.pointer("/spec/selector") {
        vec![FieldError::invalid(
            "spec.selector",
            format!("{}", new.pointer("/spec/selector").unwrap_or(&Value::Null)),
            "field is immutable",
        )]
    } else {
        vec![]
    }
}

pub fn validate_deployment_update(new: &Value, old: &Value) -> ErrorList {
    let mut errs = validate_deployment(new);
    errs.extend(validate_selector_immutable(new, old));
    errs
}

pub fn validate_replicaset(obj: &Value) -> ErrorList {
    let mut errs = vec![];
    let Some(rs) = decode_or::<ReplicaSet>(obj, &mut errs) else { return errs };
    let spec = rs.spec.unwrap_or_default();
    validate_replicas(spec.replicas, &mut errs);
    validate_selector_and_template(spec.selector.as_ref(), spec.template.as_ref(), &mut errs);
    errs
}

pub fn validate_replicaset_update(new: &Value, old: &Value) -> ErrorList {
    let mut errs = validate_replicaset(new);
    errs.extend(validate_selector_immutable(new, old));
    errs
}

pub fn validate_daemonset(obj: &Value) -> ErrorList {
    let mut errs = vec![];
    let Some(ds) = decode_or::<DaemonSet>(obj, &mut errs) else { return errs };
    let spec = ds.spec.unwrap_or_default();
    validate_selector_and_template(spec.selector.as_ref(), Some(&spec.template), &mut errs);
    errs
}

pub fn validate_statefulset(obj: &Value) -> ErrorList {
    let mut errs = vec![];
    let Some(ss) = decode_or::<StatefulSet>(obj, &mut errs) else { return errs };
    let spec = ss.spec.unwrap_or_default();
    validate_replicas(spec.replicas, &mut errs);
    validate_selector_and_template(spec.selector.as_ref(), Some(&spec.template), &mut errs);
    if let Some(p) = &spec.pod_management_policy {
        if p != "OrderedReady" && p != "Parallel" {
            errs.push(FieldError::not_supported("spec.podManagementPolicy", p, &["OrderedReady", "Parallel"]));
        }
    }
    errs
}

pub fn validate_workload_update(new: &Value, old: &Value, validate: fn(&Value) -> ErrorList) -> ErrorList {
    let mut errs = validate(new);
    errs.extend(validate_selector_immutable(new, old));
    errs
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn deploy(sel: Value, labels: Value) -> Value {
        json!({"metadata": {"name": "d"}, "spec": {"selector": sel,
            "template": {"metadata": {"labels": labels},
                         "spec": {"containers": [{"name": "c", "image": "nginx"}]}}}})
    }

    #[test]
    fn selector_must_match_template() {
        assert!(validate_deployment(&deploy(json!({"matchLabels": {"app": "a"}}), json!({"app": "a"}))).is_empty());
        let errs = validate_deployment(&deploy(json!({"matchLabels": {"app": "a"}}), json!({"app": "b"})));
        assert_eq!(errs.len(), 1);
        assert!(errs[0].to_string().contains("`selector` does not match template `labels`"));
        let errs = validate_deployment(&deploy(json!(null), json!({"app": "a"})));
        assert_eq!(errs[0].field, "spec.selector");
    }

    #[test]
    fn selector_immutable() {
        let a = deploy(json!({"matchLabels": {"app": "a"}}), json!({"app": "a", "x": "y"}));
        let b = deploy(json!({"matchLabels": {"x": "y"}}), json!({"app": "a", "x": "y"}));
        assert_eq!(validate_deployment_update(&b, &a).len(), 1);
    }
}
