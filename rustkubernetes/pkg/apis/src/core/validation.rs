//! Port of `pkg/apis/core/validation/validation.go` (commonly used subset).

use std::collections::{BTreeMap, HashSet};

use k8s_api::core::v1::{ConfigMap, Container, Namespace, Pod, PodSpec, Secret, Service, Toleration};
use k8s_api::IntOrString;
use k8s_apimachinery::quantity::parse_milli;
use k8s_apimachinery::validation::{is_dns1035_label, is_dns1123_label, is_valid_port_num, validate_labels, ErrorList, FieldError};
use serde_json::Value;

use crate::decode;

fn decode_or_err<T: serde::de::DeserializeOwned>(obj: &Value, errs: &mut ErrorList) -> Option<T> {
    match decode(obj) {
        Ok(t) => Some(t),
        Err(e) => {
            errs.push(FieldError::invalid("<object>", "", format!("unable to decode: {e}")));
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Pod
// ---------------------------------------------------------------------------

pub fn validate_pod(obj: &Value) -> ErrorList {
    let mut errs = vec![];
    let Some(pod) = decode_or_err::<Pod>(obj, &mut errs) else { return errs };
    match &pod.spec {
        Some(spec) => errs.extend(validate_pod_spec(spec, "spec")),
        None => errs.push(FieldError::required("spec", "")),
    }
    errs
}

const RESTART_POLICIES: &[&str] = &["Always", "OnFailure", "Never"];
const DNS_POLICIES: &[&str] = &["ClusterFirstWithHostNet", "ClusterFirst", "Default", "None"];
const PROTOCOLS: &[&str] = &["TCP", "UDP", "SCTP"];
const PULL_POLICIES: &[&str] = &["Always", "IfNotPresent", "Never"];

pub fn validate_pod_spec(spec: &PodSpec, path: &str) -> ErrorList {
    let mut errs = vec![];
    // volumes
    let mut volume_names = HashSet::new();
    for (i, v) in spec.volumes.iter().flatten().enumerate() {
        let p = format!("{path}.volumes[{i}].name");
        for m in is_dns1123_label(&v.name) {
            errs.push(FieldError::invalid(&p, &v.name, m));
        }
        if !volume_names.insert(v.name.clone()) {
            errs.push(FieldError::duplicate(&p, &v.name));
        }
        if v.source.is_empty() {
            errs.push(FieldError::required(format!("{path}.volumes[{i}]"), "must specify a volume type"));
        }
    }
    // containers
    if spec.containers.is_empty() {
        errs.push(FieldError::required(format!("{path}.containers"), ""));
    }
    let mut names = HashSet::new();
    for (i, c) in spec.init_containers.iter().flatten().enumerate() {
        errs.extend(validate_container(c, &format!("{path}.initContainers[{i}]"), &volume_names, &mut names, true));
    }
    for (i, c) in spec.containers.iter().enumerate() {
        errs.extend(validate_container(c, &format!("{path}.containers[{i}]"), &volume_names, &mut names, false));
    }
    if let Some(rp) = &spec.restart_policy {
        if !RESTART_POLICIES.contains(&rp.as_str()) {
            errs.push(FieldError::not_supported(format!("{path}.restartPolicy"), rp, RESTART_POLICIES));
        }
    }
    if let Some(dp) = &spec.dns_policy {
        if !DNS_POLICIES.contains(&dp.as_str()) {
            errs.push(FieldError::not_supported(format!("{path}.dnsPolicy"), dp, DNS_POLICIES));
        }
    }
    if let Some(g) = spec.termination_grace_period_seconds {
        if g < 0 {
            errs.push(FieldError::invalid(format!("{path}.terminationGracePeriodSeconds"), g, "must be greater than or equal to 0"));
        }
    }
    if let Some(d) = spec.active_deadline_seconds {
        if d < 1 {
            errs.push(FieldError::invalid(format!("{path}.activeDeadlineSeconds"), d, "must be between 1 and 2147483647, inclusive"));
        }
    }
    if let Some(ns) = &spec.node_selector {
        errs.extend(validate_labels(ns, &format!("{path}.nodeSelector")));
    }
    for (i, t) in spec.tolerations.iter().flatten().enumerate() {
        errs.extend(validate_toleration(t, &format!("{path}.tolerations[{i}]")));
    }
    if let Some(h) = &spec.hostname {
        for m in is_dns1123_label(h) {
            errs.push(FieldError::invalid(format!("{path}.hostname"), h, m));
        }
    }
    errs
}

fn validate_container(c: &Container, path: &str, volumes: &HashSet<String>, names: &mut HashSet<String>, init: bool) -> ErrorList {
    let mut errs = vec![];
    if c.name.is_empty() {
        errs.push(FieldError::required(format!("{path}.name"), ""));
    } else {
        for m in is_dns1123_label(&c.name) {
            errs.push(FieldError::invalid(format!("{path}.name"), &c.name, m));
        }
        if !names.insert(c.name.clone()) {
            errs.push(FieldError::duplicate(format!("{path}.name"), &c.name));
        }
    }
    if c.image.as_deref().unwrap_or("").trim().is_empty() {
        errs.push(FieldError::required(format!("{path}.image"), ""));
    }
    if let Some(pp) = &c.image_pull_policy {
        if !PULL_POLICIES.contains(&pp.as_str()) {
            errs.push(FieldError::not_supported(format!("{path}.imagePullPolicy"), pp, PULL_POLICIES));
        }
    }
    let mut port_names = HashSet::new();
    for (i, p) in c.ports.iter().flatten().enumerate() {
        let pp = format!("{path}.ports[{i}]");
        if !is_valid_port_num(p.container_port as i64) {
            errs.push(FieldError::invalid(format!("{pp}.containerPort"), p.container_port, "must be between 1 and 65535, inclusive"));
        }
        if let Some(hp) = p.host_port {
            if hp != 0 && !is_valid_port_num(hp as i64) {
                errs.push(FieldError::invalid(format!("{pp}.hostPort"), hp, "must be between 1 and 65535, inclusive"));
            }
        }
        if let Some(proto) = &p.protocol {
            if !PROTOCOLS.contains(&proto.as_str()) {
                errs.push(FieldError::not_supported(format!("{pp}.protocol"), proto, PROTOCOLS));
            }
        }
        if let Some(n) = &p.name {
            if n.len() > 15 {
                errs.push(FieldError::invalid(format!("{pp}.name"), n, "must be no more than 15 characters"));
            }
            if !port_names.insert(n.clone()) {
                errs.push(FieldError::duplicate(format!("{pp}.name"), n));
            }
        }
    }
    for (i, e) in c.env.iter().flatten().enumerate() {
        if e.name.is_empty() {
            errs.push(FieldError::required(format!("{path}.env[{i}].name"), ""));
        } else if e.name.contains('=') {
            errs.push(FieldError::invalid(format!("{path}.env[{i}].name"), &e.name, "must not contain '='"));
        }
    }
    for (i, m) in c.volume_mounts.iter().flatten().enumerate() {
        if !volumes.contains(&m.name) {
            errs.push(FieldError::invalid(format!("{path}.volumeMounts[{i}].name"), &m.name, "volume not found"));
        }
        if m.mount_path.is_empty() {
            errs.push(FieldError::required(format!("{path}.volumeMounts[{i}].mountPath"), ""));
        }
    }
    if let Some(res) = &c.resources {
        errs.extend(validate_resources(res.requests.as_ref(), res.limits.as_ref(), &format!("{path}.resources")));
    }
    if init && (c.liveness_probe.is_some() || c.readiness_probe.is_some()) && c.restart_policy.as_deref() != Some("Always") {
        errs.push(FieldError::forbidden(format!("{path}.livenessProbe"), "may not be set for init containers"));
    }
    if let Some(rp) = &c.restart_policy {
        if !init {
            errs.push(FieldError::forbidden(format!("{path}.restartPolicy"), "may not be set for non-init containers"));
        } else if rp != "Always" {
            errs.push(FieldError::not_supported(format!("{path}.restartPolicy"), rp, &["Always"]));
        }
    }
    errs
}

fn validate_resources(requests: Option<&BTreeMap<String, String>>, limits: Option<&BTreeMap<String, String>>, path: &str) -> ErrorList {
    let mut errs = vec![];
    for (kind, map) in [("requests", requests), ("limits", limits)] {
        for (k, v) in map.into_iter().flatten() {
            if let Err(e) = parse_milli(v) {
                errs.push(FieldError::invalid(format!("{path}.{kind}[{k}]"), v, e));
            }
        }
    }
    if let (Some(req), Some(lim)) = (requests, limits) {
        for (k, r) in req {
            if let (Some(l), Ok(rq)) = (lim.get(k), parse_milli(r)) {
                if let Ok(lv) = parse_milli(l) {
                    if rq > lv {
                        errs.push(FieldError::invalid(
                            format!("{path}.requests[{k}]"),
                            r,
                            format!("must be less than or equal to {k} limit of {l}"),
                        ));
                    }
                }
            }
        }
    }
    errs
}

fn validate_toleration(t: &Toleration, path: &str) -> ErrorList {
    let mut errs = vec![];
    let op = t.operator.as_deref().unwrap_or("Equal");
    match op {
        "Equal" => {}
        "Exists" => {
            if t.value.as_deref().is_some_and(|v| !v.is_empty()) {
                errs.push(FieldError::invalid(format!("{path}.operator"), op, "value must be empty when `operator` is 'Exists'"));
            }
        }
        _ => errs.push(FieldError::not_supported(format!("{path}.operator"), op, &["Equal", "Exists"])),
    }
    if t.key.as_deref().unwrap_or("").is_empty() && op != "Exists" {
        errs.push(FieldError::invalid(
            format!("{path}.operator"),
            op,
            "operator must be Exists when `key` is empty, which means \"match all values and all keys\"",
        ));
    }
    if let Some(e) = &t.effect {
        let ok = ["NoSchedule", "PreferNoSchedule", "NoExecute"];
        if !e.is_empty() && !ok.contains(&e.as_str()) {
            errs.push(FieldError::not_supported(format!("{path}.effect"), e, &ok));
        }
    }
    errs
}

/// `ValidatePodUpdate`: only a few spec fields are mutable.
pub fn validate_pod_update(new: &Value, old: &Value) -> ErrorList {
    let mut errs = validate_pod(new);
    let (Ok(n), Ok(o)) = (decode::<Pod>(new), decode::<Pod>(old)) else { return errs };
    let (Some(mut ns), Some(os)) = (n.spec.clone(), o.spec.clone()) else { return errs };
    // Allowed: container images, activeDeadlineSeconds, adding tolerations,
    // terminationGracePeriodSeconds (only to shorten a deleting pod).
    for (i, c) in ns.containers.iter_mut().enumerate() {
        if let Some(oc) = os.containers.get(i) {
            c.image = oc.image.clone();
        }
    }
    if let (Some(nic), Some(oic)) = (ns.init_containers.as_mut(), os.init_containers.as_ref()) {
        for (i, c) in nic.iter_mut().enumerate() {
            if let Some(oc) = oic.get(i) {
                c.image = oc.image.clone();
            }
        }
    }
    // schedulingGates may only be removed (that is how gated pods are released).
    let gates = |s: &k8s_api::core::v1::PodSpec| s.extra.get("schedulingGates").and_then(|g| g.as_array()).cloned().unwrap_or_default();
    let (new_gates, old_gates) = (gates(&ns), gates(&os));
    if new_gates.iter().all(|g| old_gates.contains(g)) {
        match os.extra.get("schedulingGates") {
            Some(g) => ns.extra.insert("schedulingGates".into(), g.clone()),
            None => ns.extra.remove("schedulingGates"),
        };
    } else {
        errs.push(FieldError::forbidden("spec.schedulingGates", "only deletion is allowed, but found new scheduling gate"));
    }
    ns.active_deadline_seconds = os.active_deadline_seconds;
    ns.termination_grace_period_seconds = os.termination_grace_period_seconds;
    let old_tols = os.tolerations.clone().unwrap_or_default();
    if let Some(nt) = &ns.tolerations {
        if old_tols.iter().all(|t| nt.contains(t)) {
            ns.tolerations = os.tolerations.clone();
        }
    }
    if ns != os {
        errs.push(FieldError::forbidden(
            "spec",
            "pod updates may not change fields other than `spec.containers[*].image`,`spec.initContainers[*].image`,`spec.activeDeadlineSeconds`,`spec.tolerations` (only additions to existing tolerations),`spec.terminationGracePeriodSeconds` (allow it to be set to 1 if it was previously negative)",
        ));
    }
    errs
}

const POD_PHASES: &[&str] = &["Pending", "Running", "Succeeded", "Failed", "Unknown"];

pub fn validate_pod_status_update(new: &Value, _old: &Value) -> ErrorList {
    let mut errs = vec![];
    if let Some(p) = new.pointer("/status/phase").and_then(Value::as_str) {
        if !POD_PHASES.contains(&p) {
            errs.push(FieldError::not_supported("status.phase", p, POD_PHASES));
        }
    }
    errs
}

// ---------------------------------------------------------------------------
// Namespace / ConfigMap / Secret / Node
// ---------------------------------------------------------------------------

pub fn validate_namespace(obj: &Value) -> ErrorList {
    let mut errs = vec![];
    let Some(ns) = decode_or_err::<Namespace>(obj, &mut errs) else { return errs };
    for m in is_dns1123_label(ns.metadata.name()) {
        errs.push(FieldError::invalid("metadata.name", ns.metadata.name(), m));
    }
    errs
}

fn validate_data_keys<'a>(keys: impl Iterator<Item = &'a String>, path: &str) -> ErrorList {
    let mut errs = vec![];
    for k in keys {
        let ok = !k.is_empty() && k.len() <= 253 && k.chars().all(|c| c.is_ascii_alphanumeric() || "-._".contains(c));
        if !ok {
            errs.push(FieldError::invalid(path, k, "a valid config key must consist of alphanumeric characters, '-', '_' or '.'"));
        }
    }
    errs
}

pub fn validate_configmap(obj: &Value) -> ErrorList {
    let mut errs = vec![];
    let Some(cm) = decode_or_err::<ConfigMap>(obj, &mut errs) else { return errs };
    let data = cm.data.clone().unwrap_or_default();
    let bin = cm.binary_data.clone().unwrap_or_default();
    errs.extend(validate_data_keys(data.keys(), "data"));
    errs.extend(validate_data_keys(bin.keys(), "binaryData"));
    for k in data.keys() {
        if bin.contains_key(k) {
            errs.push(FieldError::invalid("data", k, "duplicate of key present in binaryData"));
        }
    }
    let size: usize =
        data.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>() + bin.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>();
    if size > 1024 * 1024 {
        errs.push(FieldError::too_long("", 1024 * 1024));
    }
    errs
}

pub fn validate_secret(obj: &Value) -> ErrorList {
    use base64::Engine;
    let mut errs = vec![];
    let Some(s) = decode_or_err::<Secret>(obj, &mut errs) else { return errs };
    let data = s.data.clone().unwrap_or_default();
    errs.extend(validate_data_keys(data.keys(), "data"));
    for (k, v) in &data {
        if base64::engine::general_purpose::STANDARD.decode(v).is_err() {
            errs.push(FieldError::invalid(format!("data[{k}]"), "<secret contents redacted>", "illegal base64 data"));
        }
    }
    errs
}

/// Immutable ConfigMaps/Secrets may not change their data.
pub fn validate_immutable_data(new: &Value, old: &Value) -> ErrorList {
    let mut errs = vec![];
    if old.get("immutable").and_then(Value::as_bool) == Some(true) {
        if new.get("immutable").and_then(Value::as_bool) != Some(true) {
            errs.push(FieldError::forbidden("immutable", "field is immutable when `immutable` is set"));
        }
        for k in ["data", "binaryData"] {
            if new.get(k) != old.get(k) {
                errs.push(FieldError::forbidden(k, "field is immutable when `immutable` is set"));
            }
        }
    }
    errs
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

const SERVICE_TYPES: &[&str] = &["ClusterIP", "NodePort", "LoadBalancer", "ExternalName"];

pub fn validate_service(obj: &Value) -> ErrorList {
    let mut errs = vec![];
    let Some(svc) = decode_or_err::<Service>(obj, &mut errs) else { return errs };
    for m in is_dns1035_label(svc.metadata.name()) {
        errs.push(FieldError::invalid("metadata.name", svc.metadata.name(), m));
    }
    let spec = svc.spec.unwrap_or_default();
    let type_ = spec.type_.clone().unwrap_or_else(|| "ClusterIP".into());
    if !SERVICE_TYPES.contains(&type_.as_str()) {
        errs.push(FieldError::not_supported("spec.type", &type_, SERVICE_TYPES));
    }
    let headless = spec.cluster_ip.as_deref() == Some("None");
    let ports = spec.ports.clone().unwrap_or_default();
    if type_ == "ExternalName" {
        if spec.external_name.as_deref().unwrap_or("").is_empty() {
            errs.push(FieldError::required("spec.externalName", ""));
        }
    } else if ports.is_empty() && !headless {
        errs.push(FieldError::required("spec.ports", ""));
    }
    let mut names = HashSet::new();
    let mut seen = HashSet::new();
    for (i, p) in ports.iter().enumerate() {
        let pp = format!("spec.ports[{i}]");
        if ports.len() > 1 {
            match &p.name {
                None => errs.push(FieldError::required(format!("{pp}.name"), "")),
                Some(n) => {
                    if !names.insert(n.clone()) {
                        errs.push(FieldError::duplicate(format!("{pp}.name"), n));
                    }
                }
            }
        }
        if !is_valid_port_num(p.port as i64) {
            errs.push(FieldError::invalid(format!("{pp}.port"), p.port, "must be between 1 and 65535, inclusive"));
        }
        let proto = p.protocol.clone().unwrap_or_else(|| "TCP".into());
        if !PROTOCOLS.contains(&proto.as_str()) {
            errs.push(FieldError::not_supported(format!("{pp}.protocol"), &proto, PROTOCOLS));
        }
        if !seen.insert((p.port, proto.clone())) {
            errs.push(FieldError::duplicate(pp.to_string(), format!("{}/{}", p.port, proto)));
        }
        match &p.target_port {
            Some(IntOrString::Int(t)) if !is_valid_port_num(*t) => {
                errs.push(FieldError::invalid(format!("{pp}.targetPort"), t, "must be between 1 and 65535, inclusive"));
            }
            Some(IntOrString::String(s)) if s.is_empty() || s.len() > 15 => {
                errs.push(FieldError::invalid(format!("{pp}.targetPort"), s, "must be a valid port name"));
            }
            _ => {}
        }
        if let Some(np) = p.node_port {
            if type_ != "NodePort" && type_ != "LoadBalancer" && np != 0 {
                errs.push(FieldError::forbidden(format!("{pp}.nodePort"), "may not be used when `type` is 'ClusterIP'"));
            }
        }
    }
    if let Some(sel) = &spec.selector {
        errs.extend(validate_labels(sel, "spec.selector"));
    }
    errs
}

pub fn validate_service_update(new: &Value, old: &Value) -> ErrorList {
    let mut errs = validate_service(new);
    let old_ip = old.pointer("/spec/clusterIP").and_then(Value::as_str).unwrap_or("");
    let new_ip = new.pointer("/spec/clusterIP").and_then(Value::as_str).unwrap_or("");
    let new_type = new.pointer("/spec/type").and_then(Value::as_str).unwrap_or("ClusterIP");
    if !old_ip.is_empty() && new_ip != old_ip && new_type != "ExternalName" {
        errs.push(FieldError::invalid("spec.clusterIPs[0]", new_ip, "may not change once set"));
    }
    errs
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn invalid_pods() {
        let errs = validate_pod(&json!({"metadata": {"name": "p"}, "spec": {"containers": []}}));
        assert_eq!(errs[0].to_string(), "spec.containers: Required value");
        let errs = validate_pod(&json!({"spec": {"containers": [
            {"name": "a", "image": "x"}, {"name": "a"}]}}));
        assert!(errs.iter().any(|e| e.field == "spec.containers[1].name"));
        assert!(errs.iter().any(|e| e.field == "spec.containers[1].image"));
        let errs = validate_pod(&json!({"spec": {"containers": [
            {"name": "a", "image": "x", "ports": [{"containerPort": 70000}],
             "volumeMounts": [{"name": "nope", "mountPath": "/x"}]}]}}));
        assert_eq!(errs.len(), 2);
    }

    #[test]
    fn pod_update_rules() {
        let old = json!({"spec": {"containers": [{"name": "a", "image": "x:1"}]}});
        let ok = json!({"spec": {"containers": [{"name": "a", "image": "x:2"}]}});
        assert!(validate_pod_update(&ok, &old).is_empty());
        let bad = json!({"spec": {"containers": [{"name": "a", "image": "x:1"}], "nodeName": "n1"}});
        assert_eq!(validate_pod_update(&bad, &old).len(), 1);
        let gated = json!({"spec": {"containers": [{"name": "a", "image": "x:1"}], "schedulingGates": [{"name": "g"}]}});
        assert!(validate_pod_update(&old, &gated).is_empty(), "removing gates is allowed");
        assert!(!validate_pod_update(&gated, &old).is_empty(), "adding gates is not");
    }

    #[test]
    fn services() {
        assert!(validate_service(&json!({"metadata": {"name": "web"}, "spec": {"ports": [{"port": 80}]}})).is_empty());
        let errs = validate_service(&json!({"metadata": {"name": "1web"}, "spec": {}}));
        assert_eq!(errs.len(), 2);
    }
}
