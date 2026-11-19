//! Port of `k8s.io/apiextensions-api/pkg/apis/apiextensions/v1` (types,
//! defaults) and `apiextensions-apiserver/pkg/apihelpers` (condition and
//! version helpers). CRDs are handled as JSON; these helpers read the fields.

use serde_json::{json, Value};

pub const GROUP: &str = "apiextensions.k8s.io";
pub const VERSION: &str = "v1";
/// `CustomResourceCleanupFinalizer`: deletes all instances before the CRD goes away.
pub const CLEANUP_FINALIZER: &str = "customresourcecleanup.apiextensions.k8s.io";
/// Annotation required on CRDs in protected (`*.k8s.io`, `*.kubernetes.io`) groups.
pub const API_APPROVAL_ANNOTATION: &str = "api-approved.kubernetes.io";

pub const ESTABLISHED: &str = "Established";
pub const NAMES_ACCEPTED: &str = "NamesAccepted";
pub const TERMINATING: &str = "Terminating";
pub const NON_STRUCTURAL_SCHEMA: &str = "NonStructuralSchema";
pub const API_APPROVAL_CONDITION: &str = "KubernetesAPIApprovalPolicyConformant";

pub fn str_at<'a>(v: &'a Value, p: &str) -> &'a str {
    v.pointer(p).and_then(Value::as_str).unwrap_or("")
}

/// `SetObjectDefaults_CustomResourceDefinition`
pub fn set_defaults(crd: &mut Value) {
    let kind = str_at(crd, "/spec/names/kind").to_string();
    if !crd["spec"].is_object() {
        return;
    }
    if !crd["spec"]["names"].is_object() {
        crd["spec"]["names"] = json!({});
    }
    let names = &mut crd["spec"]["names"];
    if names.get("singular").and_then(Value::as_str).unwrap_or("").is_empty() && !kind.is_empty() {
        names["singular"] = json!(kind.to_lowercase());
    }
    if names.get("listKind").and_then(Value::as_str).unwrap_or("").is_empty() && !kind.is_empty() {
        names["listKind"] = json!(format!("{kind}List"));
    }
    if crd["spec"].get("conversion").is_none_or(Value::is_null) {
        crd["spec"]["conversion"] = json!({"strategy": "None"});
    }
    if let Some(port) = crd.pointer_mut("/spec/conversion/webhook/clientConfig/service") {
        if port.get("port").is_none() {
            port["port"] = json!(443);
        }
    }
}

pub fn versions(crd: &Value) -> Vec<Value> {
    crd.pointer("/spec/versions").and_then(Value::as_array).cloned().unwrap_or_default()
}

/// `GetCRDStorageVersion`
pub fn storage_version(crd: &Value) -> Option<String> {
    versions(crd).iter().find(|v| v["storage"] == true).map(|v| str_at(v, "/name").to_string())
}

/// Served version names, sorted by kube-aware version priority (`SortedByGroupAndVersion`).
pub fn served_versions(crd: &Value) -> Vec<String> {
    let mut vs: Vec<String> = versions(crd).iter().filter(|v| v["served"] == true).map(|v| str_at(v, "/name").to_string()).collect();
    vs.sort_by(|a, b| compare_kube_aware_version(b, a).then_with(|| a.cmp(b)));
    vs
}

pub fn version<'a>(crd: &'a Value, name: &str) -> Option<&'a Value> {
    crd.pointer("/spec/versions").and_then(Value::as_array)?.iter().find(|v| str_at(v, "/name") == name)
}

/// `CompareKubeAwareVersionStrings`: GA > beta > alpha > other, higher numbers first.
pub fn compare_kube_aware_version(a: &str, b: &str) -> std::cmp::Ordering {
    fn rank(v: &str) -> (u8, u64, u64) {
        let Some(rest) = v.strip_prefix('v') else { return (0, 0, 0) };
        let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        let Ok(major) = rest[..end].parse::<u64>() else { return (0, 0, 0) };
        let tail = &rest[end..];
        if tail.is_empty() {
            return (3, major, 0);
        }
        for (pre, r) in [("beta", 2), ("alpha", 1)] {
            if let Some(n) = tail.strip_prefix(pre) {
                if let Ok(minor) = n.parse::<u64>() {
                    return (r, major, minor);
                }
            }
        }
        (0, 0, 0)
    }
    let (ra, rb) = (rank(a), rank(b));
    if ra.0 == 0 && rb.0 == 0 {
        return b.cmp(a);
    }
    ra.cmp(&rb)
}

pub fn condition<'a>(crd: &'a Value, ty: &str) -> Option<&'a Value> {
    crd.pointer("/status/conditions").and_then(Value::as_array)?.iter().find(|c| c["type"] == ty)
}

pub fn is_condition_true(crd: &Value, ty: &str) -> bool {
    condition(crd, ty).is_some_and(|c| c["status"] == "True")
}

/// `SetCRDCondition`: replaces a condition, keeping `lastTransitionTime` when the status is unchanged.
pub fn set_condition(crd: &mut Value, ty: &str, status: &str, reason: &str, message: &str) {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    if !crd["status"].is_object() {
        crd["status"] = json!({});
    }
    let conds = crd["status"].as_object_mut().unwrap().entry("conditions").or_insert_with(|| json!([]));
    if !conds.is_array() {
        *conds = json!([]);
    }
    let list = conds.as_array_mut().unwrap();
    let new = json!({"type": ty, "status": status, "lastTransitionTime": now, "reason": reason, "message": message});
    match list.iter_mut().find(|c| c["type"] == ty) {
        Some(c) => {
            let mut n = new;
            if c["status"] == status {
                n["lastTransitionTime"] = c["lastTransitionTime"].clone();
            }
            *c = n;
        }
        None => list.push(new),
    }
}

pub fn remove_condition(crd: &mut Value, ty: &str) {
    if let Some(list) = crd.pointer_mut("/status/conditions").and_then(Value::as_array_mut) {
        list.retain(|c| c["type"] != ty);
    }
}

/// `IsProtectedCommunityGroup`: `k8s.io`, `kubernetes.io` and their subdomains.
pub fn is_protected_group(group: &str) -> bool {
    group == "k8s.io" || group == "kubernetes.io" || group.ends_with(".k8s.io") || group.ends_with(".kubernetes.io")
}

/// `GetAPIApprovalState`: (state, reason) for protected groups.
pub fn api_approval_state(annotations: &Value) -> (&'static str, String) {
    let Some(v) = annotations.get(API_APPROVAL_ANNOTATION).and_then(Value::as_str) else {
        return ("Missing", format!("protected groups must have approval annotation {API_APPROVAL_ANNOTATION:?}, see https://github.com/kubernetes/enhancements/pull/1111"));
    };
    if v.starts_with("unapproved") {
        return ("Unapproved", format!("not approved: {v:?}"));
    }
    if url_like(v) {
        return ("Approved", format!("approved in {v}"));
    }
    ("Invalid", format!("protected groups must have approval annotation {API_APPROVAL_ANNOTATION:?} with either a URL or a reason starting with \"unapproved\", see https://github.com/kubernetes/enhancements/pull/1111"))
}

fn url_like(v: &str) -> bool {
    (v.starts_with("https://") || v.starts_with("http://")) && v.split("://").nth(1).is_some_and(|h| !h.is_empty())
}

/// The schema of a version (`openAPIV3Schema`), if any.
pub fn schema<'a>(crd: &'a Value, version: &str) -> Option<&'a Value> {
    self::version(crd, version)?.pointer("/schema/openAPIV3Schema").filter(|s| s.is_object())
}
