//! Port of `pkg/apis/apps/v1/defaults.go`.

use serde_json::{json, Value};

use crate::core::defaults::set_pod_spec_defaults;
use crate::{child, default_field};

fn template_defaults(spec: &mut Value) {
    let template = child(spec, "template");
    set_pod_spec_defaults(child(template, "spec"));
}

/// `SetDefaults_Deployment`
pub fn set_deployment_defaults(obj: &mut Value) {
    let spec = child(obj, "spec");
    default_field(spec, "replicas", json!(1));
    default_field(spec, "revisionHistoryLimit", json!(10));
    default_field(spec, "progressDeadlineSeconds", json!(600));
    let strategy = child(spec, "strategy");
    default_field(strategy, "type", json!("RollingUpdate"));
    if strategy["type"] == "RollingUpdate" {
        let ru = child(strategy, "rollingUpdate");
        default_field(ru, "maxUnavailable", json!("25%"));
        default_field(ru, "maxSurge", json!("25%"));
    }
    template_defaults(spec);
}

/// `SetDefaults_ReplicaSet`
pub fn set_replicaset_defaults(obj: &mut Value) {
    let spec = child(obj, "spec");
    default_field(spec, "replicas", json!(1));
    template_defaults(spec);
}

/// `SetDefaults_DaemonSet`
pub fn set_daemonset_defaults(obj: &mut Value) {
    let spec = child(obj, "spec");
    default_field(spec, "revisionHistoryLimit", json!(10));
    let us = child(spec, "updateStrategy");
    default_field(us, "type", json!("RollingUpdate"));
    if us["type"] == "RollingUpdate" {
        let ru = child(us, "rollingUpdate");
        default_field(ru, "maxUnavailable", json!(1));
        default_field(ru, "maxSurge", json!(0));
    }
    template_defaults(spec);
}

/// `SetDefaults_StatefulSet`
pub fn set_statefulset_defaults(obj: &mut Value) {
    let spec = child(obj, "spec");
    default_field(spec, "replicas", json!(1));
    default_field(spec, "podManagementPolicy", json!("OrderedReady"));
    default_field(spec, "revisionHistoryLimit", json!(10));
    let us = child(spec, "updateStrategy");
    default_field(us, "type", json!("RollingUpdate"));
    template_defaults(spec);
}
