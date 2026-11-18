//! Strategies for simple core resources: configmaps, secrets, serviceaccounts,
//! endpoints, events.

use k8s_apiserver::table::{age_cell, Column};
use kube_apis::core::{defaults, validation};
use serde_json::{json, Value};

use crate::generic::{str_at, SimpleStrategy};

fn data_count(obj: &Value) -> usize {
    ["data", "binaryData"].iter().filter_map(|k| obj.get(*k).and_then(Value::as_object)).map(|m| m.len()).sum()
}

pub fn configmap() -> SimpleStrategy {
    SimpleStrategy::new(false)
        .validate(validation::validate_configmap)
        .validate_update(|n, o| {
            let mut e = validation::validate_configmap(n);
            e.extend(validation::validate_immutable_data(n, o));
            e
        })
        .table(
            || vec![Column::new("Data", "string", ""), Column::new("Age", "string", "")],
            |o| vec![json!(data_count(o).to_string()), age_cell(o)],
        )
}

pub fn secret() -> SimpleStrategy {
    SimpleStrategy::new(false)
        .defaults(defaults::set_secret_defaults)
        .validate(validation::validate_secret)
        .validate_update(|n, o| {
            let mut e = validation::validate_secret(n);
            e.extend(validation::validate_immutable_data(n, o));
            if n.get("type") != o.get("type") {
                e.push(k8s_apimachinery::validation::FieldError::invalid("type", str_at(n, "/type"), "field is immutable"));
            }
            e
        })
        .table(
            || vec![Column::new("Type", "string", ""), Column::new("Data", "string", ""), Column::new("Age", "string", "")],
            |o| vec![json!(str_at(o, "/type")), json!(data_count(o).to_string()), age_cell(o)],
        )
}

pub fn serviceaccount() -> SimpleStrategy {
    SimpleStrategy::new(false).table(
        || vec![Column::new("Secrets", "string", ""), Column::new("Age", "string", "")],
        |o| vec![json!(o.get("secrets").and_then(Value::as_array).map(Vec::len).unwrap_or(0).to_string()), age_cell(o)],
    )
}

pub fn endpoints() -> SimpleStrategy {
    SimpleStrategy::new(false).table(
        || vec![Column::new("Endpoints", "string", ""), Column::new("Age", "string", "")],
        |o| {
            let mut eps = vec![];
            for s in o.get("subsets").and_then(Value::as_array).into_iter().flatten() {
                let ports: Vec<i64> = s["ports"].as_array().into_iter().flatten().filter_map(|p| p["port"].as_i64()).collect();
                for a in s["addresses"].as_array().into_iter().flatten() {
                    let ip = a["ip"].as_str().unwrap_or("");
                    if ports.is_empty() {
                        eps.push(ip.to_string());
                    }
                    for p in &ports {
                        eps.push(format!("{ip}:{p}"));
                    }
                }
            }
            let shown = if eps.len() > 3 { format!("{} + {} more...", eps[..3].join(","), eps.len() - 3) } else { eps.join(",") };
            vec![json!(if shown.is_empty() { "<none>".to_string() } else { shown }), age_cell(o)]
        },
    )
}

pub fn event() -> SimpleStrategy {
    SimpleStrategy::new(false).table(
        || {
            vec![
                Column::new("Last Seen", "string", ""),
                Column::new("Type", "string", ""),
                Column::new("Reason", "string", ""),
                Column::new("Object", "string", ""),
                Column::new("Message", "string", ""),
            ]
        },
        |o| {
            let last = ["/lastTimestamp", "/eventTime", "/metadata/creationTimestamp"]
                .iter()
                .map(|p| str_at(o, p))
                .find(|s| !s.is_empty())
                .unwrap_or("");
            vec![
                json!(k8s_apimachinery::meta::human_age(last)),
                json!(str_at(o, "/type")),
                json!(str_at(o, "/reason")),
                json!(format!("{}/{}", str_at(o, "/involvedObject/kind").to_lowercase(), str_at(o, "/involvedObject/name"))),
                json!(str_at(o, "/message")),
            ]
        },
    )
}

pub fn lease() -> SimpleStrategy {
    SimpleStrategy::new(false).table(
        || vec![Column::new("Holder", "string", ""), Column::new("Age", "string", "")],
        |o| vec![json!(str_at(o, "/spec/holderIdentity")), age_cell(o)],
    )
}
