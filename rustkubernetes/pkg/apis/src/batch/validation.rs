//! Port of `pkg/apis/batch/validation/validation.go` (subset).

use k8s_api::batch::v1::{CronJob, Job, JobSpec};
use k8s_apimachinery::labels::Selector;
use k8s_apimachinery::validation::{ErrorList, FieldError};
use serde_json::Value;

use crate::core::validation::validate_pod_spec;
use crate::decode;

fn validate_job_spec(spec: &JobSpec, path: &str, check_selector: bool) -> ErrorList {
    let mut errs = vec![];
    for (name, v) in [("parallelism", spec.parallelism), ("completions", spec.completions), ("backoffLimit", spec.backoff_limit)] {
        if let Some(v) = v {
            if v < 0 {
                errs.push(FieldError::invalid(format!("{path}.{name}"), v, "must be greater than or equal to 0"));
            }
        }
    }
    if let Some(d) = spec.active_deadline_seconds {
        if d <= 0 {
            errs.push(FieldError::invalid(format!("{path}.activeDeadlineSeconds"), d, "must be greater than 0"));
        }
    }
    let mode = spec.completion_mode.as_deref().unwrap_or("NonIndexed");
    if mode != "NonIndexed" && mode != "Indexed" {
        errs.push(FieldError::not_supported(format!("{path}.completionMode"), mode, &["NonIndexed", "Indexed"]));
    }
    if mode == "Indexed" && spec.completions.is_none() {
        errs.push(FieldError::required(format!("{path}.completions"), "when completion mode is Indexed"));
    }
    if check_selector {
        match &spec.selector {
            None => errs.push(FieldError::required(format!("{path}.selector"), "")),
            Some(sel) => {
                if let Ok(s) = Selector::from_label_selector(sel) {
                    let labels = spec.template.metadata.as_ref().map(|m| m.labels()).unwrap_or_default();
                    if !s.matches(&labels) {
                        errs.push(FieldError::invalid(
                            format!("{path}.template.metadata.labels"),
                            format!("{labels:?}"),
                            "`selector` does not match template `labels`",
                        ));
                    }
                }
            }
        }
    }
    match &spec.template.spec {
        None => errs.push(FieldError::required(format!("{path}.template.spec"), "")),
        Some(ps) => {
            errs.extend(validate_pod_spec(ps, &format!("{path}.template.spec")));
            let rp = ps.restart_policy.as_deref().unwrap_or("Always");
            if rp != "OnFailure" && rp != "Never" {
                errs.push(FieldError::not_supported(format!("{path}.template.spec.restartPolicy"), rp, &["OnFailure", "Never"]));
            }
        }
    }
    errs
}

pub fn validate_job(obj: &Value) -> ErrorList {
    match decode::<Job>(obj) {
        Ok(j) => validate_job_spec(&j.spec.unwrap_or_default(), "spec", true),
        Err(e) => vec![FieldError::invalid("<object>", "", e)],
    }
}

/// Selector, template and completion mode are immutable; parallelism, suspend etc. may change.
pub fn validate_job_update(new: &Value, old: &Value) -> ErrorList {
    let mut errs = validate_job(new);
    for f in ["selector", "completionMode"] {
        if new.pointer(&format!("/spec/{f}")) != old.pointer(&format!("/spec/{f}")) {
            errs.push(FieldError::invalid(format!("spec.{f}"), "", "field is immutable"));
        }
    }
    let strip = |v: &Value| {
        let mut t = v.pointer("/spec/template").cloned().unwrap_or(Value::Null);
        // Scheduling directives may be changed while a job is suspended; keep it simple: images too.
        if let Some(cs) = t.pointer_mut("/spec/containers").and_then(Value::as_array_mut) {
            for c in cs {
                if let Some(o) = c.as_object_mut() {
                    o.remove("image");
                }
            }
        }
        t
    };
    if strip(new) != strip(old) {
        errs.push(FieldError::invalid("spec.template", "", "field is immutable"));
    }
    errs
}

/// Validates a standard 5-field cron schedule (or a `@daily`-style macro).
pub fn validate_schedule(s: &str) -> Result<(), String> {
    crate::batch::validation::cron_fields(s).map(|_| ())
}

/// Splits a schedule into its five fields, expanding macros.
pub fn cron_fields(s: &str) -> Result<[String; 5], String> {
    let s = s.trim();
    let expanded = match s {
        "@yearly" | "@annually" => "0 0 1 1 *",
        "@monthly" => "0 0 1 * *",
        "@weekly" => "0 0 * * 0",
        "@daily" | "@midnight" => "0 0 * * *",
        "@hourly" => "0 * * * *",
        other => other,
    };
    let parts: Vec<&str> = expanded.split_whitespace().collect();
    if parts.len() != 5 {
        return Err(format!("expected exactly 5 fields, found {}: {s}", parts.len()));
    }
    let allowed = |c: char| c.is_ascii_alphanumeric() || "*/,-?".contains(c);
    for p in &parts {
        if !p.chars().all(allowed) {
            return Err(format!("unparseable field {p:?} in schedule {s:?}"));
        }
    }
    Ok([parts[0].into(), parts[1].into(), parts[2].into(), parts[3].into(), parts[4].into()])
}

pub fn validate_cronjob(obj: &Value) -> ErrorList {
    let cj = match decode::<CronJob>(obj) {
        Ok(c) => c,
        Err(e) => return vec![FieldError::invalid("<object>", "", e)],
    };
    let mut errs = vec![];
    let name = cj.metadata.name();
    if name.len() > 52 {
        errs.push(FieldError::invalid("metadata.name", name, "must be no more than 52 characters"));
    }
    let spec = cj.spec.unwrap_or_default();
    if spec.schedule.is_empty() {
        errs.push(FieldError::required("spec.schedule", ""));
    } else if let Err(e) = validate_schedule(&spec.schedule) {
        errs.push(FieldError::invalid("spec.schedule", &spec.schedule, e));
    }
    if let Some(tz) = &spec.time_zone {
        if tz != "UTC" && tz != "Etc/UTC" {
            errs.push(FieldError::not_supported("spec.timeZone", tz, &["UTC", "Etc/UTC"]));
        }
    }
    let cp = spec.concurrency_policy.as_deref().unwrap_or("Allow");
    if !["Allow", "Forbid", "Replace"].contains(&cp) {
        errs.push(FieldError::not_supported("spec.concurrencyPolicy", cp, &["Allow", "Forbid", "Replace"]));
    }
    if let Some(js) = &spec.job_template.spec {
        errs.extend(validate_job_spec(js, "spec.jobTemplate.spec", false));
    } else {
        errs.push(FieldError::required("spec.jobTemplate.spec", ""));
    }
    errs
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn job_restart_policy() {
        let job = json!({"spec": {"selector": {"matchLabels": {"a": "b"}},
            "template": {"metadata": {"labels": {"a": "b"}},
                         "spec": {"restartPolicy": "Always", "containers": [{"name": "c", "image": "x"}]}}}});
        let errs = validate_job(&job);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "spec.template.spec.restartPolicy");
    }

    #[test]
    fn schedules() {
        assert!(validate_schedule("*/5 * * * *").is_ok());
        assert!(validate_schedule("@hourly").is_ok());
        assert!(validate_schedule("* * *").is_err());
    }
}
