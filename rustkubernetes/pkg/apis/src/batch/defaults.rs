//! Port of `pkg/apis/batch/v1/defaults.go` and the Job selector generation of
//! `pkg/registry/batch/job/strategy.go`.

use serde_json::{json, Value};

use crate::core::defaults::set_pod_spec_defaults;
use crate::{child, default_field};

/// Label set on a Job's pods and used as its generated selector.
pub const CONTROLLER_UID_LABEL: &str = "batch.kubernetes.io/controller-uid";
pub const JOB_NAME_LABEL: &str = "batch.kubernetes.io/job-name";
/// Legacy labels still set for compatibility.
pub const LEGACY_CONTROLLER_UID_LABEL: &str = "controller-uid";
pub const LEGACY_JOB_NAME_LABEL: &str = "job-name";

/// `SetDefaults_JobSpec` on a job spec value.
pub fn set_job_spec_defaults(spec: &mut Value) {
    let has_completions = spec.get("completions").is_some_and(|v| !v.is_null());
    let has_parallelism = spec.get("parallelism").is_some_and(|v| !v.is_null());
    if !has_completions && !has_parallelism {
        spec["completions"] = json!(1);
        spec["parallelism"] = json!(1);
    }
    default_field(spec, "parallelism", json!(1));
    default_field(spec, "backoffLimit", json!(6));
    default_field(spec, "completionMode", json!("NonIndexed"));
    default_field(spec, "suspend", json!(false));
    default_field(spec, "manualSelector", json!(false));
    default_field(spec, "podReplacementPolicy", json!("TerminatingOrFailed"));
    let template = child(spec, "template");
    set_pod_spec_defaults(child(template, "spec"));
}

/// Job defaults plus the generated selector (`manualSelector: false`), which
/// needs the object's UID (already assigned when this runs on create).
pub fn set_job_defaults(job: &mut Value) {
    let uid = job.pointer("/metadata/uid").and_then(Value::as_str).unwrap_or("").to_string();
    let name = job.pointer("/metadata/name").and_then(Value::as_str).unwrap_or("").to_string();
    let spec = child(job, "spec");
    set_job_spec_defaults(spec);
    let manual = spec.get("manualSelector").and_then(Value::as_bool).unwrap_or(false);
    if !manual && !uid.is_empty() && spec.pointer("/selector").is_none_or(Value::is_null) {
        spec["selector"] = json!({"matchLabels": {CONTROLLER_UID_LABEL: uid}});
        let labels = child(child(child(spec, "template"), "metadata"), "labels");
        for (k, v) in [
            (CONTROLLER_UID_LABEL, uid.as_str()),
            (JOB_NAME_LABEL, name.as_str()),
            (LEGACY_CONTROLLER_UID_LABEL, uid.as_str()),
            (LEGACY_JOB_NAME_LABEL, name.as_str()),
        ] {
            labels[k] = json!(v);
        }
    }
}

/// `SetDefaults_CronJob`
pub fn set_cronjob_defaults(cj: &mut Value) {
    let spec = child(cj, "spec");
    default_field(spec, "concurrencyPolicy", json!("Allow"));
    default_field(spec, "suspend", json!(false));
    default_field(spec, "successfulJobsHistoryLimit", json!(3));
    default_field(spec, "failedJobsHistoryLimit", json!(1));
    let jt = child(spec, "jobTemplate");
    set_job_spec_defaults(child(jt, "spec"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_defaults_and_selector() {
        let mut job = json!({"metadata": {"name": "pi", "uid": "u-1"},
            "spec": {"template": {"spec": {"restartPolicy": "Never", "containers": [{"name": "c", "image": "perl"}]}}}});
        set_job_defaults(&mut job);
        assert_eq!(job["spec"]["completions"], 1);
        assert_eq!(job["spec"]["parallelism"], 1);
        assert_eq!(job["spec"]["backoffLimit"], 6);
        assert_eq!(job["spec"]["selector"]["matchLabels"][CONTROLLER_UID_LABEL], "u-1");
        assert_eq!(job["spec"]["template"]["metadata"]["labels"][JOB_NAME_LABEL], "pi");
        // parallelism only: completions stays unset (work-queue job)
        let mut spec = json!({"parallelism": 3});
        set_job_spec_defaults(&mut spec);
        assert!(spec.get("completions").is_none());
    }
}
