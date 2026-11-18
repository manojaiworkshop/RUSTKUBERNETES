//! Port of `pkg/apis/core/v1/defaults.go` (`SetDefaults_*`).

use serde_json::{json, Value};

use crate::{child, default_field};

/// `SetDefaults_PodSpec` + `SetDefaults_Container` for a pod spec value.
pub fn set_pod_spec_defaults(spec: &mut Value) {
    if !spec.is_object() {
        return;
    }
    default_field(spec, "restartPolicy", json!("Always"));
    default_field(spec, "dnsPolicy", json!("ClusterFirst"));
    default_field(spec, "terminationGracePeriodSeconds", json!(30));
    default_field(spec, "schedulerName", json!("default-scheduler"));
    default_field(spec, "securityContext", json!({}));
    default_field(spec, "enableServiceLinks", json!(true));
    default_field(spec, "serviceAccountName", json!("default"));
    for list in ["containers", "initContainers"] {
        if let Some(cs) = spec.get_mut(list).and_then(Value::as_array_mut) {
            for c in cs {
                set_container_defaults(c);
            }
        }
    }
}

/// `SetDefaults_Container`
pub fn set_container_defaults(c: &mut Value) {
    if !c.is_object() {
        return;
    }
    let image = c.get("image").and_then(Value::as_str).unwrap_or("").to_string();
    default_field(c, "imagePullPolicy", json!(default_pull_policy(&image)));
    default_field(c, "terminationMessagePath", json!("/dev/termination-log"));
    default_field(c, "terminationMessagePolicy", json!("File"));
    default_field(c, "resources", json!({}));
    if let Some(ports) = c.get_mut("ports").and_then(Value::as_array_mut) {
        for p in ports {
            default_field(p, "protocol", json!("TCP"));
        }
    }
}

/// `Always` for `:latest` or untagged images, `IfNotPresent` otherwise.
pub fn default_pull_policy(image: &str) -> &'static str {
    let without_digest = image.split('@').next().unwrap_or("");
    let last = without_digest.rsplit('/').next().unwrap_or("");
    match last.split_once(':') {
        _ if image.contains('@') => "IfNotPresent",
        Some((_, "latest")) | None => "Always",
        Some(_) => "IfNotPresent",
    }
}

/// Defaults for a Pod object (spec + initial status).
pub fn set_pod_defaults(pod: &mut Value) {
    set_pod_spec_defaults(child(pod, "spec"));
    let qos = super::helper::qos_class_value(pod.get("spec").unwrap_or(&Value::Null));
    let status = child(pod, "status");
    default_field(status, "phase", json!("Pending"));
    default_field(status, "qosClass", json!(qos));
}

/// `SetDefaults_Service` / `SetDefaults_ServicePort`.
pub fn set_service_defaults(svc: &mut Value) {
    let spec = child(svc, "spec");
    default_field(spec, "type", json!("ClusterIP"));
    default_field(spec, "sessionAffinity", json!("None"));
    if spec["sessionAffinity"] == "ClientIP" {
        // DefaultClientIPServiceAffinitySeconds = 3h
        let cfg = child(spec, "sessionAffinityConfig");
        let cip = child(cfg, "clientIP");
        default_field(cip, "timeoutSeconds", json!(10800));
    }
    let type_ = spec["type"].as_str().unwrap_or("ClusterIP").to_string();
    if type_ != "ExternalName" {
        default_field(spec, "internalTrafficPolicy", json!("Cluster"));
    }
    if type_ == "NodePort" || type_ == "LoadBalancer" {
        default_field(spec, "externalTrafficPolicy", json!("Cluster"));
    }
    if let Some(ports) = spec.get_mut("ports").and_then(Value::as_array_mut) {
        for p in ports {
            default_field(p, "protocol", json!("TCP"));
            if let Some(port) = p.get("port").cloned() {
                default_field(p, "targetPort", port);
            }
        }
    }
}

/// `SetDefaults_Namespace` / `SetDefaults_NamespaceStatus`.
pub fn set_namespace_defaults(ns: &mut Value) {
    let spec = child(ns, "spec");
    default_field(spec, "finalizers", json!(["kubernetes"]));
    let status = child(ns, "status");
    default_field(status, "phase", json!("Active"));
}

/// `SetDefaults_Secret`: `type: Opaque`; `stringData` is merged into `data` (base64).
pub fn set_secret_defaults(secret: &mut Value) {
    use base64::Engine;
    default_field(secret, "type", json!("Opaque"));
    if let Some(Value::Object(sd)) = secret.as_object_mut().and_then(|m| m.remove("stringData")) {
        let data = child(secret, "data");
        for (k, v) in sd {
            let raw = v.as_str().unwrap_or("").as_bytes().to_vec();
            data[k] = json!(base64::engine::general_purpose::STANDARD.encode(raw));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_policy() {
        assert_eq!(default_pull_policy("nginx"), "Always");
        assert_eq!(default_pull_policy("nginx:latest"), "Always");
        assert_eq!(default_pull_policy("nginx:1.27"), "IfNotPresent");
        assert_eq!(default_pull_policy("localhost:5000/app"), "Always");
        assert_eq!(default_pull_policy("localhost:5000/app:v1"), "IfNotPresent");
        assert_eq!(default_pull_policy("nginx@sha256:abc"), "IfNotPresent");
    }

    #[test]
    fn pod_defaults() {
        let mut pod = json!({"spec": {"containers": [{"name": "c", "image": "nginx", "ports": [{"containerPort": 80}]}]}});
        set_pod_defaults(&mut pod);
        assert_eq!(pod["spec"]["restartPolicy"], "Always");
        assert_eq!(pod["spec"]["containers"][0]["imagePullPolicy"], "Always");
        assert_eq!(pod["spec"]["containers"][0]["ports"][0]["protocol"], "TCP");
        assert_eq!(pod["status"]["phase"], "Pending");
        assert_eq!(pod["status"]["qosClass"], "BestEffort");
    }

    #[test]
    fn secret_string_data() {
        let mut s = json!({"stringData": {"user": "admin"}});
        set_secret_defaults(&mut s);
        assert_eq!(s["data"]["user"], "YWRtaW4=");
        assert!(s.get("stringData").is_none());
        assert_eq!(s["type"], "Opaque");
    }
}
