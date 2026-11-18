//! Strategies for the `apps` group (`pkg/registry/apps/*`).

use k8s_apiserver::table::{age_cell, Column};
use kube_apis::apps::{defaults, validation};
use serde_json::{json, Value};

use crate::generic::{format_labels, int_at, SimpleStrategy};

fn containers_and_images(o: &Value) -> (String, String) {
    let cs = o.pointer("/spec/template/spec/containers").and_then(Value::as_array).cloned().unwrap_or_default();
    let names = cs.iter().filter_map(|c| c["name"].as_str()).collect::<Vec<_>>().join(",");
    let images = cs.iter().filter_map(|c| c["image"].as_str()).collect::<Vec<_>>().join(",");
    (names, images)
}

fn wide_columns() -> Vec<Column> {
    vec![
        Column::new("Containers", "string", "Names of each container in the template.").wide(),
        Column::new("Images", "string", "Images referenced by each container in the template.").wide(),
        Column::new("Selector", "string", "Label selector for pods.").wide(),
    ]
}

fn wide_cells(o: &Value) -> Vec<Value> {
    let (names, images) = containers_and_images(o);
    vec![json!(names), json!(images), json!(format_labels(o.pointer("/spec/selector/matchLabels")))]
}

pub fn deployment() -> SimpleStrategy {
    SimpleStrategy::new(true)
        .defaults(defaults::set_deployment_defaults)
        .validate(validation::validate_deployment)
        .validate_update(validation::validate_deployment_update)
        .table(
            || {
                let mut c = vec![
                    Column::new("Ready", "string", "Number of the pod with ready state"),
                    Column::new(
                        "Up-to-date",
                        "string",
                        "Total number of non-terminated pods targeted by this deployment that have the desired template spec.",
                    ),
                    Column::new(
                        "Available",
                        "string",
                        "Total number of available pods (ready for at least minReadySeconds) targeted by this deployment.",
                    ),
                    Column::new("Age", "string", ""),
                ];
                c.extend(wide_columns());
                c
            },
            |o| {
                let mut c = vec![
                    json!(format!("{}/{}", int_at(o, "/status/readyReplicas"), int_at(o, "/spec/replicas"))),
                    json!(int_at(o, "/status/updatedReplicas")),
                    json!(int_at(o, "/status/availableReplicas")),
                    age_cell(o),
                ];
                c.extend(wide_cells(o));
                c
            },
        )
}

pub fn replicaset() -> SimpleStrategy {
    SimpleStrategy::new(true)
        .defaults(defaults::set_replicaset_defaults)
        .validate(validation::validate_replicaset)
        .validate_update(validation::validate_replicaset_update)
        .table(
            || {
                let mut c = vec![
                    Column::new("Desired", "integer", "Number of desired pods."),
                    Column::new("Current", "integer", "Number of actually running pods."),
                    Column::new("Ready", "integer", "Number of ready pods."),
                    Column::new("Age", "string", ""),
                ];
                c.extend(wide_columns());
                c
            },
            |o| {
                let mut c = vec![
                    json!(int_at(o, "/spec/replicas")),
                    json!(int_at(o, "/status/replicas")),
                    json!(int_at(o, "/status/readyReplicas")),
                    age_cell(o),
                ];
                c.extend(wide_cells(o));
                c
            },
        )
}

pub fn daemonset() -> SimpleStrategy {
    SimpleStrategy::new(true)
        .defaults(defaults::set_daemonset_defaults)
        .validate(validation::validate_daemonset)
        .validate_update(|n, o| validation::validate_workload_update(n, o, validation::validate_daemonset))
        .table(
            || {
                vec![
                    Column::new("Desired", "integer", ""),
                    Column::new("Current", "integer", ""),
                    Column::new("Ready", "integer", ""),
                    Column::new("Up-to-date", "integer", ""),
                    Column::new("Available", "integer", ""),
                    Column::new("Node Selector", "string", ""),
                    Column::new("Age", "string", ""),
                ]
            },
            |o| {
                vec![
                    json!(int_at(o, "/status/desiredNumberScheduled")),
                    json!(int_at(o, "/status/currentNumberScheduled")),
                    json!(int_at(o, "/status/numberReady")),
                    json!(int_at(o, "/status/updatedNumberScheduled")),
                    json!(int_at(o, "/status/numberAvailable")),
                    json!(format_labels(o.pointer("/spec/template/spec/nodeSelector"))),
                    age_cell(o),
                ]
            },
        )
}

pub fn statefulset() -> SimpleStrategy {
    SimpleStrategy::new(true)
        .defaults(defaults::set_statefulset_defaults)
        .validate(validation::validate_statefulset)
        .validate_update(|n, o| validation::validate_workload_update(n, o, validation::validate_statefulset))
        .table(
            || vec![Column::new("Ready", "string", ""), Column::new("Age", "string", "")],
            |o| vec![json!(format!("{}/{}", int_at(o, "/status/readyReplicas"), int_at(o, "/spec/replicas"))), age_cell(o)],
        )
}
