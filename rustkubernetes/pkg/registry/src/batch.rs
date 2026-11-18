//! Strategies for `batch/v1` (`pkg/registry/batch/{job,cronjob}`) and
//! `discovery.k8s.io/v1` EndpointSlices.

use k8s_apimachinery::meta::{human_age, parse_time};
use k8s_apiserver::table::{age_cell, Column};
use kube_apis::batch::{defaults, validation};
use serde_json::{json, Value};

use crate::generic::{int_at, str_at, SimpleStrategy};

fn job_status(o: &Value) -> &'static str {
    let conds = o.pointer("/status/conditions").and_then(Value::as_array).cloned().unwrap_or_default();
    let has = |t: &str| conds.iter().any(|c| c["type"] == t && c["status"] == "True");
    if has("Complete") {
        "Complete"
    } else if has("Failed") {
        "Failed"
    } else if has("Suspended") {
        "Suspended"
    } else if o.deletion_timestamp_str().is_some() {
        "Terminating"
    } else {
        "Running"
    }
}

trait DelTs {
    fn deletion_timestamp_str(&self) -> Option<&str>;
}
impl DelTs for Value {
    fn deletion_timestamp_str(&self) -> Option<&str> {
        self.pointer("/metadata/deletionTimestamp").and_then(Value::as_str)
    }
}

fn duration(o: &Value) -> String {
    let Some(start) = parse_time(str_at(o, "/status/startTime")) else { return String::new() };
    let end = parse_time(str_at(o, "/status/completionTime")).unwrap_or_else(chrono::Utc::now);
    let secs = (end - start).num_seconds().max(0);
    if secs < 120 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

pub fn job() -> SimpleStrategy {
    SimpleStrategy::new(true)
        .defaults(defaults::set_job_defaults)
        .validate(validation::validate_job)
        .validate_update(validation::validate_job_update)
        .table(
            || {
                vec![
                    Column::new("Status", "string", ""),
                    Column::new("Completions", "string", "The desired number of successfully finished pods the job should be run with."),
                    Column::new("Duration", "string", "Time required to complete the job."),
                    Column::new("Age", "string", ""),
                ]
            },
            |o| {
                let completions = match o.pointer("/spec/completions").and_then(Value::as_i64) {
                    Some(c) => format!("{}/{c}", int_at(o, "/status/succeeded")),
                    None => format!("{}/1 of {}", int_at(o, "/status/succeeded"), int_at(o, "/spec/parallelism")),
                };
                vec![json!(job_status(o)), json!(completions), json!(duration(o)), age_cell(o)]
            },
        )
}

pub fn cronjob() -> SimpleStrategy {
    SimpleStrategy::new(true).defaults(defaults::set_cronjob_defaults).validate(validation::validate_cronjob).table(
        || {
            vec![
                Column::new("Schedule", "string", ""),
                Column::new("Timezone", "string", ""),
                Column::new("Suspend", "boolean", ""),
                Column::new("Active", "integer", ""),
                Column::new("Last Schedule", "string", ""),
                Column::new("Age", "string", ""),
            ]
        },
        |o| {
            let last = str_at(o, "/status/lastScheduleTime");
            vec![
                json!(str_at(o, "/spec/schedule")),
                json!(o.pointer("/spec/timeZone").and_then(Value::as_str).unwrap_or("<none>")),
                json!(o.pointer("/spec/suspend").and_then(Value::as_bool).unwrap_or(false)),
                json!(o.pointer("/status/active").and_then(Value::as_array).map(Vec::len).unwrap_or(0)),
                json!(if last.is_empty() { "<none>".to_string() } else { human_age(last) }),
                age_cell(o),
            ]
        },
    )
}

pub fn endpointslice() -> SimpleStrategy {
    SimpleStrategy::new(false).table(
        || {
            vec![
                Column::new("AddressType", "string", ""),
                Column::new("Ports", "string", ""),
                Column::new("Endpoints", "string", ""),
                Column::new("Age", "string", ""),
            ]
        },
        |o| {
            let ports: Vec<String> =
                o["ports"].as_array().into_iter().flatten().filter_map(|p| p["port"].as_i64().map(|n| n.to_string())).collect();
            let addrs: Vec<String> = o["endpoints"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|e| e["addresses"].as_array().cloned().unwrap_or_default())
                .filter_map(|a| a.as_str().map(String::from))
                .collect();
            let eps = match addrs.len() {
                0 => "<unset>".to_string(),
                n if n > 3 => format!("{} + {} more...", addrs[..3].join(","), n - 3),
                _ => addrs.join(","),
            };
            vec![
                json!(str_at(o, "/addressType")),
                json!(if ports.is_empty() { "<unset>".to_string() } else { ports.join(",") }),
                json!(eps),
                age_cell(o),
            ]
        },
    )
}

/// `scheduling.k8s.io/v1` PriorityClass (`pkg/registry/scheduling/priorityclass`).
pub fn priorityclass() -> SimpleStrategy {
    SimpleStrategy::new(false)
        .defaults(|o| {
            if o.get("preemptionPolicy").is_none_or(Value::is_null) {
                o["preemptionPolicy"] = json!("PreemptLowerPriority");
            }
        })
        .validate(|o| {
            let mut errs = vec![];
            let p = o["preemptionPolicy"].as_str().unwrap_or("PreemptLowerPriority");
            if p != "PreemptLowerPriority" && p != "Never" {
                errs.push(k8s_apimachinery::validation::FieldError::not_supported(
                    "preemptionPolicy",
                    p,
                    &["PreemptLowerPriority", "Never"],
                ));
            }
            if !o["value"].is_i64() {
                errs.push(k8s_apimachinery::validation::FieldError::required("value", ""));
            }
            errs
        })
        .validate_update(|n, o| {
            let mut errs = vec![];
            for f in ["value", "preemptionPolicy"] {
                if n.get(f) != o.get(f) {
                    errs.push(k8s_apimachinery::validation::FieldError::forbidden(f, "may not be changed in an update."));
                }
            }
            errs
        })
        .table(
            || {
                vec![
                    Column::new("Value", "integer", ""),
                    Column::new("Global-Default", "boolean", ""),
                    Column::new("Preemption-Policy", "string", ""),
                    Column::new("Age", "string", ""),
                ]
            },
            |o| {
                vec![
                    json!(o["value"].as_i64().unwrap_or(0)),
                    json!(o["globalDefault"].as_bool().unwrap_or(false)),
                    json!(str_at(o, "/preemptionPolicy")),
                    age_cell(o),
                ]
            },
        )
}
