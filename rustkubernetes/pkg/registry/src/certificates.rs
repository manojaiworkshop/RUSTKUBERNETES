//! Port of `pkg/registry/certificates/certificates` (`strategy.go`) and
//! `pkg/apis/certificates/validation`: CertificateSigningRequests with the main,
//! `status` and `approval` strategies, and the `kubectl get csr` printer.
//!
//! * create: `spec.username/uid/groups/extra` come from the requesting user and
//!   the status is cleared (no pre-approved requests);
//! * main update: only metadata changes;
//! * `/status`: certificate and non-approval conditions (signers);
//! * `/approval`: only the conditions (approvers).

use async_trait::async_trait;
use base64::Engine;
use k8s_apimachinery::meta::{human_duration, now_rfc3339};
use k8s_apimachinery::validation::{ErrorList, FieldError};
use k8s_apiserver::authentication::current_user;
use k8s_apiserver::registry::Strategy;
use k8s_apiserver::table::{age_cell, Column};
use kube_apis::certificates::{parse_csr, validate_certificate_pem, validate_signer_name, ALL_VALID_USAGES};
use serde_json::{json, Value};

use crate::generic::str_at;

const APPROVED: &str = "Approved";
const DENIED: &str = "Denied";
const FAILED: &str = "Failed";
const LEGACY_UNKNOWN: &str = "kubernetes.io/legacy-unknown";

pub struct CsrStrategy;

/// Go's `%#v` of a `[]byte` (how `field.Invalid` prints `spec.request`).
fn go_bytes(b: &[u8]) -> String {
    format!("[]byte{{{}}}", b.iter().map(|x| format!("0x{x:x}")).collect::<Vec<_>>().join(", "))
}

fn b64(v: Option<&Value>) -> Vec<u8> {
    v.and_then(Value::as_str).and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok()).unwrap_or_default()
}

fn conditions(obj: &Value) -> Vec<Value> {
    obj.pointer("/status/conditions").and_then(Value::as_array).cloned().unwrap_or_default()
}

fn of_type(obj: &Value, t: &str) -> Vec<Value> {
    conditions(obj).into_iter().filter(|c| str_at(c, "/type") == t).collect()
}

fn usages(obj: &Value) -> Vec<String> {
    obj.pointer("/spec/usages").and_then(Value::as_array).into_iter().flatten().filter_map(|u| u.as_str().map(str::to_string)).collect()
}

/// `certificateValidationOptions`
#[derive(Default)]
struct Opts {
    allow_setting_approval_conditions: bool,
    allow_setting_certificate: bool,
    allow_legacy_signer_name: bool,
    allow_duplicate_condition_types: bool,
    allow_empty_condition_type: bool,
    allow_arbitrary_certificate: bool,
    allow_unknown_usages: bool,
    allow_duplicate_usages: bool,
}

fn has_dup<T: Ord + Clone>(v: &[T]) -> bool {
    let mut s = std::collections::BTreeSet::new();
    v.iter().any(|x| !s.insert(x.clone()))
}

/// `getValidationOptions`: tolerate existing data that newer rules would reject.
fn opts(new: &Value, old: Option<&Value>) -> Opts {
    let Some(old) = old else { return Opts::default() };
    let types: Vec<String> = conditions(old).iter().map(|c| str_at(c, "/type").to_string()).collect();
    let old_cert = b64(old.pointer("/status/certificate"));
    let old_usages = usages(old);
    Opts {
        allow_legacy_signer_name: str_at(old, "/spec/signerName") == LEGACY_UNKNOWN,
        allow_duplicate_condition_types: has_dup(&types),
        allow_empty_condition_type: types.iter().any(String::is_empty),
        allow_arbitrary_certificate: b64(new.pointer("/status/certificate")) == old_cert || validate_certificate_pem(&old_cert).is_err(),
        allow_unknown_usages: old_usages.iter().any(|u| !ALL_VALID_USAGES.contains(&u.as_str())),
        allow_duplicate_usages: has_dup(&old_usages),
        ..Default::default()
    }
}

/// `validateCertificateSigningRequest`
fn validate_csr(obj: &Value, o: &Opts) -> ErrorList {
    let mut errs = vec![];
    let request = b64(obj.pointer("/spec/request"));
    if let Err(e) = parse_csr(&request) {
        errs.push(FieldError::invalid_raw("spec.request", go_bytes(&request), e));
    }
    let us = usages(obj);
    if us.is_empty() {
        errs.push(FieldError::required("spec.usages", ""));
    }
    if !o.allow_unknown_usages {
        for (i, u) in us.iter().enumerate() {
            if !ALL_VALID_USAGES.contains(&u.as_str()) {
                errs.push(FieldError::not_supported(format!("spec.usages[{i}]"), u, ALL_VALID_USAGES));
            }
        }
    }
    if !o.allow_duplicate_usages {
        let mut seen = std::collections::BTreeSet::new();
        for (i, u) in us.iter().enumerate() {
            if !seen.insert(u) {
                errs.push(FieldError::duplicate(format!("spec.usages[{i}]"), u));
            }
        }
    }
    let signer = str_at(obj, "/spec/signerName");
    if !o.allow_legacy_signer_name && signer == LEGACY_UNKNOWN {
        errs.push(FieldError::invalid("spec.signerName", signer, "the legacy signerName is not allowed via this API version"));
    } else {
        errs.extend(validate_signer_name("spec.signerName", signer));
    }
    if let Some(s) = obj.pointer("/spec/expirationSeconds").and_then(Value::as_i64) {
        if s < 600 {
            errs.push(FieldError::invalid_raw(
                "spec.expirationSeconds",
                s.to_string(),
                "may not specify a duration less than 600 seconds (10 minutes)",
            ));
        }
    }
    // validateConditions
    let mut seen = std::collections::BTreeSet::new();
    for (i, c) in conditions(obj).iter().enumerate() {
        let t = str_at(c, "/type");
        if !o.allow_empty_condition_type && t.is_empty() {
            errs.push(FieldError::required(format!("status.conditions[{i}].type"), ""));
        }
        let allowed: &[&str] = if [APPROVED, DENIED, FAILED].contains(&t) { &["True"] } else { &["False", "True", "Unknown"] };
        let st = str_at(c, "/status");
        if st.is_empty() {
            errs.push(FieldError::required(format!("status.conditions[{i}].status"), ""));
        } else if !allowed.contains(&st) {
            errs.push(FieldError::not_supported(format!("status.conditions[{i}].status"), st, allowed));
        }
        if !o.allow_duplicate_condition_types && !seen.insert(t.to_string()) {
            errs.push(FieldError::duplicate(format!("status.conditions[{i}].type"), t));
        }
    }
    if !o.allow_arbitrary_certificate {
        if let Err(e) = validate_certificate_pem(&b64(obj.pointer("/status/certificate"))) {
            errs.push(FieldError::invalid("status.certificate", "<certificate data>", e));
        }
    }
    errs
}

/// `validateCertificateSigningRequestUpdate` (status and approval).
fn validate_csr_update(new: &Value, old: &Value, o: Opts) -> ErrorList {
    let mut errs = vec![];
    if new.get("spec") != old.get("spec") {
        errs.push(FieldError::invalid("spec", "", "field is immutable"));
    }
    errs.extend(validate_csr(new, &o));
    for t in [APPROVED, DENIED, FAILED] {
        if of_type(new, t).len() < of_type(old, t).len() {
            errs.push(FieldError::forbidden("status.conditions", format!("updates may not remove a condition of type {t:?}")));
        }
    }
    if !o.allow_setting_approval_conditions {
        for t in [APPROVED, DENIED] {
            let (oc, nc) = (of_type(old, t), of_type(new, t));
            if nc.len() > oc.len() {
                errs.push(FieldError::forbidden("status.conditions", format!("updates may not add a condition of type {t:?}")));
            } else if nc.len() == oc.len() && nc != oc {
                errs.push(FieldError::forbidden("status.conditions", format!("updates may not modify a condition of type {t:?}")));
            }
        }
    }
    let (nc, oc) = (b64(new.pointer("/status/certificate")), b64(old.pointer("/status/certificate")));
    if nc != oc {
        if !o.allow_setting_certificate {
            errs.push(FieldError::forbidden("status.certificate", "updates may not set certificate content"));
        } else if !oc.is_empty() {
            errs.push(FieldError::forbidden("status.certificate", "updates may not modify existing certificate content"));
        }
    }
    errs
}

/// `populateConditionTimestamps`
fn populate_condition_timestamps(new: &mut Value, old: &Value) {
    let now = now_rfc3339();
    let old_conds = conditions(old);
    let Some(conds) = new.pointer_mut("/status/conditions").and_then(Value::as_array_mut) else { return };
    for c in conds.iter_mut() {
        if str_at(c, "/lastUpdateTime").is_empty() {
            c["lastUpdateTime"] = json!(now);
        }
        if str_at(c, "/lastTransitionTime").is_empty() {
            let prev = old_conds.iter().find(|o| {
                str_at(o, "/type") == str_at(c, "/type")
                    && str_at(o, "/status") == str_at(c, "/status")
                    && !str_at(o, "/lastTransitionTime").is_empty()
            });
            c["lastTransitionTime"] = json!(prev.map(|o| str_at(o, "/lastTransitionTime").to_string()).unwrap_or_else(|| now.clone()));
        }
    }
}

/// `preserveConditionInstances`: `/status` may not touch Approved/Denied.
fn preserve_condition_instances(new: &mut Value, old: &Value, t: &str) {
    let old_idx: Vec<Value> = of_type(old, t);
    let Some(conds) = new.pointer_mut("/status/conditions").and_then(Value::as_array_mut) else { return };
    let new_idx: Vec<usize> = conds.iter().enumerate().filter(|(_, c)| str_at(c, "/type") == t).map(|(i, _)| i).collect();
    if new_idx.len() != old_idx.len() {
        return;
    }
    for (i, o) in new_idx.into_iter().zip(old_idx) {
        conds[i] = o;
    }
}

fn set_spec_from(new: &mut Value, old: &Value) {
    match old.get("spec") {
        Some(s) => new["spec"] = s.clone(),
        None => {
            if let Some(m) = new.as_object_mut() {
                m.remove("spec");
            }
        }
    }
}

#[async_trait]
impl Strategy for CsrStrategy {
    /// `PrepareForCreate`
    fn prepare_for_create(&self, obj: &mut Value) {
        if let Some(spec) = obj.get_mut("spec").and_then(Value::as_object_mut) {
            for k in ["username", "uid", "groups", "extra"] {
                spec.remove(k);
            }
            if let Some(u) = current_user() {
                if !u.name.is_empty() {
                    spec.insert("username".into(), json!(u.name));
                }
                if !u.uid.is_empty() {
                    spec.insert("uid".into(), json!(u.uid));
                }
                if !u.groups.is_empty() {
                    spec.insert("groups".into(), json!(u.groups));
                }
                if !u.extra.is_empty() {
                    spec.insert("extra".into(), json!(u.extra));
                }
            }
        }
        obj["status"] = json!({});
    }

    /// `PrepareForUpdate`: spec and status are not changed by a main update.
    fn prepare_for_update(&self, new: &mut Value, old: &Value) {
        set_spec_from(new, old);
        match old.get("status") {
            Some(s) => new["status"] = s.clone(),
            None => {
                if let Some(m) = new.as_object_mut() {
                    m.remove("status");
                }
            }
        }
    }

    fn prepare_for_subresource_update(&self, sub: &str, new: &mut Value, old: &Value) {
        set_spec_from(new, old);
        if sub == "approval" {
            // csrApprovalStrategy: only the conditions change.
            populate_condition_timestamps(new, old);
            let conds = new.pointer("/status/conditions").cloned();
            let mut status = old.get("status").cloned().unwrap_or(json!({}));
            match conds {
                Some(c) => status["conditions"] = c,
                None => {
                    if let Some(m) = status.as_object_mut() {
                        m.remove("conditions");
                    }
                }
            }
            new["status"] = status;
        } else {
            // csrStatusStrategy
            preserve_condition_instances(new, old, APPROVED);
            preserve_condition_instances(new, old, DENIED);
            populate_condition_timestamps(new, old);
        }
    }

    fn validate(&self, obj: &Value) -> ErrorList {
        validate_csr(obj, &Opts::default())
    }

    /// `ValidateCertificateSigningRequestUpdate`: spec and status were reset from old.
    fn validate_update(&self, new: &Value, old: &Value) -> ErrorList {
        let mut errs = vec![];
        if new.get("spec") != old.get("spec") {
            errs.push(FieldError::invalid("spec", "", "field is immutable"));
        }
        if new.get("status") != old.get("status") {
            errs.push(FieldError::invalid("status", "", "field is immutable"));
        }
        errs
    }

    fn validate_status_update(&self, new: &Value, old: &Value) -> ErrorList {
        let mut o = opts(new, Some(old));
        o.allow_setting_certificate = true;
        validate_csr_update(new, old, o)
    }

    fn validate_approval_update(&self, new: &Value, old: &Value) -> ErrorList {
        let mut o = opts(new, Some(old));
        o.allow_setting_approval_conditions = true;
        validate_csr_update(new, old, o)
    }

    /// `ValidateCertificateRequestName`: any name.
    fn validate_name(&self, _name: &str) -> Vec<String> {
        vec![]
    }

    fn table_columns(&self) -> Vec<Column> {
        vec![
            Column::new("Age", "string", "CreationTimestamp is a timestamp representing the server time when this object was created."),
            Column::new("SignerName", "string", "Requested signer for the request."),
            Column::new("Requestor", "string", "Base64-encoded PKCS#10 CSR data"),
            Column::new(
                "RequestedDuration",
                "string",
                "expirationSeconds is the requested duration of validity of the issued certificate.",
            ),
            Column::new("Condition", "string", "Conditions applied to the request, such as approval or denial."),
        ]
    }

    fn table_cells(&self, obj: &Value) -> Vec<Value> {
        let signer = str_at(obj, "/spec/signerName");
        let duration =
            obj.pointer("/spec/expirationSeconds").and_then(Value::as_i64).map(human_duration).unwrap_or_else(|| "<none>".into());
        vec![
            age_cell(obj),
            json!(if signer.is_empty() { "<none>" } else { signer }),
            json!(str_at(obj, "/spec/username")),
            json!(duration),
            json!(csr_status(obj)),
        ]
    }
}

/// `extractCSRStatus`
pub fn csr_status(obj: &Value) -> String {
    let has = |t: &str| !of_type(obj, t).is_empty();
    let mut s = if has(DENIED) {
        "Denied"
    } else if has(APPROVED) {
        "Approved"
    } else {
        "Pending"
    }
    .to_string();
    if has(FAILED) {
        s.push_str(",Failed");
    }
    if obj.pointer("/status/certificate").and_then(Value::as_str).is_some_and(|c| !c.is_empty()) {
        s.push_str(",Issued");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn csr_pem() -> String {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut p = rcgen::CertificateParams::default();
        p.distinguished_name.push(rcgen::DnType::CommonName, "bob");
        p.serialize_request(&key).unwrap().pem().unwrap()
    }

    fn csr() -> Value {
        let req = base64::engine::general_purpose::STANDARD.encode(csr_pem());
        json!({"metadata": {"name": "c"}, "spec": {"request": req, "signerName": "kubernetes.io/kube-apiserver-client",
            "usages": ["client auth"], "username": "mallory"}})
    }

    #[test]
    fn create_validation() {
        let s = CsrStrategy;
        let mut c = csr();
        s.prepare_for_create(&mut c);
        assert!(c["spec"].get("username").is_none(), "user-supplied identity is dropped");
        assert_eq!(c["status"], json!({}));
        assert!(s.validate(&c).is_empty(), "{:?}", s.validate(&c));
        let mut bad = c.clone();
        bad["spec"]["request"] = json!(base64::engine::general_purpose::STANDARD.encode("x"));
        bad["spec"]["expirationSeconds"] = json!(300);
        bad["spec"]["signerName"] = json!("kubernetes.io/legacy-unknown");
        bad["spec"]["usages"] = json!(["client auth", "bogus", "client auth"]);
        let msgs: Vec<String> = s.validate(&bad).iter().map(|e| e.to_string()).collect();
        assert!(
            msgs.contains(&"spec.request: Invalid value: []byte{0x78}: PEM block type must be CERTIFICATE REQUEST".to_string()),
            "{msgs:?}"
        );
        assert!(msgs
            .iter()
            .any(|m| m == "spec.expirationSeconds: Invalid value: 300: may not specify a duration less than 600 seconds (10 minutes)"));
        assert!(msgs.iter().any(|m| m.contains("the legacy signerName is not allowed via this API version")));
        assert!(msgs.iter().any(|m| m.starts_with("spec.usages[1]: Unsupported value: \"bogus\"")));
        assert!(msgs.iter().any(|m| m == "spec.usages[2]: Duplicate value: \"client auth\""));
    }

    #[test]
    fn approval_and_status() {
        let s = CsrStrategy;
        let mut old = csr();
        s.prepare_for_create(&mut old);
        // approval: conditions only.
        let mut a = old.clone();
        a["status"] = json!({"conditions": [{"type": "Approved", "status": "True", "reason": "KubectlApprove"}], "certificate": "eA=="});
        s.prepare_for_subresource_update("approval", &mut a, &old);
        assert!(a["status"].get("certificate").is_none());
        assert!(!str_at(&a, "/status/conditions/0/lastUpdateTime").is_empty());
        assert!(s.validate_approval_update(&a, &old).is_empty());
        assert_eq!(csr_status(&a), "Approved");
        // status may not add approval conditions...
        let mut st = old.clone();
        st["status"] = json!({"conditions": [{"type": "Denied", "status": "True"}]});
        s.prepare_for_subresource_update("status", &mut st, &old);
        assert!(s
            .validate_status_update(&st, &old)
            .iter()
            .any(|e| e.to_string().contains("updates may not add a condition of type \"Denied\"")));
        // ...but may set the certificate once.
        let ca = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["x".into()]).unwrap().self_signed(&ca).unwrap().pem();
        let mut issued = a.clone();
        issued["status"]["certificate"] = json!(base64::engine::general_purpose::STANDARD.encode(&cert));
        s.prepare_for_subresource_update("status", &mut issued, &a);
        assert!(s.validate_status_update(&issued, &a).is_empty(), "{:?}", s.validate_status_update(&issued, &a));
        assert_eq!(csr_status(&issued), "Approved,Issued");
        let mut again = issued.clone();
        again["status"]["certificate"] = json!(base64::engine::general_purpose::STANDARD.encode(cert.repeat(2)));
        assert!(s
            .validate_status_update(&again, &issued)
            .iter()
            .any(|e| e.to_string().contains("updates may not modify existing certificate content")));
        // approval may not remove Approved.
        let mut un = issued.clone();
        un["status"]["conditions"] = json!([]);
        s.prepare_for_subresource_update("approval", &mut un, &issued);
        assert!(s
            .validate_approval_update(&un, &issued)
            .iter()
            .any(|e| e.to_string().contains("may not remove a condition of type \"Approved\"")));
    }
}
