//! Small node plugins: `SchedulingGates`, `NodeUnschedulable`, `NodeName`,
//! `NodePorts`, `TaintToleration`, `ImageLocality`.

use k8s_api::core::v1::{Pod, Taint};

use super::tolerates;
use crate::framework::types::wanted_host_ports;
use crate::framework::{CycleState, NodeInfo, Normalize, Plugin, Status};

pub const SCHEDULING_GATES: &str = "SchedulingGates";
pub const NODE_UNSCHEDULABLE: &str = "NodeUnschedulable";
pub const NODE_NAME: &str = "NodeName";
pub const NODE_PORTS: &str = "NodePorts";
pub const TAINT_TOLERATION: &str = "TaintToleration";
pub const IMAGE_LOCALITY: &str = "ImageLocality";

/// PreEnqueue: pods with `spec.schedulingGates` wait outside the active queue.
pub struct SchedulingGates;

impl Plugin for SchedulingGates {
    fn name(&self) -> &'static str {
        SCHEDULING_GATES
    }
    fn pre_enqueue(&self, pod: &Pod) -> Status {
        let gates = pod.spec.as_ref().and_then(|s| s.extra.get("schedulingGates")).and_then(|g| g.as_array()).map(Vec::len).unwrap_or(0);
        if gates > 0 {
            Status::unresolvable(format!("waiting for scheduling gates: {gates}"))
        } else {
            Status::success()
        }
    }
}

/// Filter: `spec.unschedulable` nodes only accept pods tolerating the unschedulable taint.
pub struct NodeUnschedulable;

impl Plugin for NodeUnschedulable {
    fn name(&self) -> &'static str {
        NODE_UNSCHEDULABLE
    }
    fn filter(&self, _: &CycleState, pod: &Pod, node: &NodeInfo) -> Status {
        if !node.node.spec.as_ref().and_then(|s| s.unschedulable).unwrap_or(false) {
            return Status::success();
        }
        let taint = Taint { key: "node.kubernetes.io/unschedulable".into(), value: None, effect: "NoSchedule".into(), time_added: None };
        let tols = pod.spec.as_ref().and_then(|s| s.tolerations.clone()).unwrap_or_default();
        if tols.iter().any(|t| tolerates(t, &taint)) {
            Status::success()
        } else {
            Status::unresolvable("node(s) were unschedulable")
        }
    }
}

/// Filter: honours a pre-set `spec.nodeName`.
pub struct NodeName;

impl Plugin for NodeName {
    fn name(&self) -> &'static str {
        NODE_NAME
    }
    fn filter(&self, _: &CycleState, pod: &Pod, node: &NodeInfo) -> Status {
        match pod.spec.as_ref().and_then(|s| s.node_name.as_deref()).filter(|n| !n.is_empty()) {
            Some(n) if n != node.name() => Status::unresolvable("node(s) didn't match the requested node name"),
            _ => Status::success(),
        }
    }
}

/// Filter: requested `hostPort`s must be free on the node.
pub struct NodePorts;

impl Plugin for NodePorts {
    fn name(&self) -> &'static str {
        NODE_PORTS
    }
    fn filter(&self, _: &CycleState, pod: &Pod, node: &NodeInfo) -> Status {
        for (ip, proto, port) in wanted_host_ports(pod) {
            let conflict = node
                .used_ports
                .iter()
                .any(|(uip, uproto, uport)| *uport == port && *uproto == proto && (uip == &ip || uip == "0.0.0.0" || ip == "0.0.0.0"));
            if conflict {
                return Status::unschedulable("node(s) didn't have free ports for the requested pod ports");
            }
        }
        Status::success()
    }
}

/// Filter: `NoSchedule`/`NoExecute` taints must be tolerated.
/// Score: fewer untolerated `PreferNoSchedule` taints is better.
pub struct TaintToleration;

impl Plugin for TaintToleration {
    fn name(&self) -> &'static str {
        TAINT_TOLERATION
    }
    fn filter(&self, _: &CycleState, pod: &Pod, node: &NodeInfo) -> Status {
        let taints = node.node.spec.as_ref().and_then(|s| s.taints.clone()).unwrap_or_default();
        let tols = pod.spec.as_ref().and_then(|s| s.tolerations.clone()).unwrap_or_default();
        for t in taints.iter().filter(|t| t.effect == "NoSchedule" || t.effect == "NoExecute") {
            if !tols.iter().any(|tol| tolerates(tol, t)) {
                return Status::unresolvable(format!("node(s) had untolerated taint {{{}: {}}}", t.key, t.value.as_deref().unwrap_or("")));
            }
        }
        Status::success()
    }
    fn score(&self, _: &CycleState, pod: &Pod, node: &NodeInfo) -> Option<i64> {
        let taints = node.node.spec.as_ref().and_then(|s| s.taints.clone()).unwrap_or_default();
        let tols: Vec<_> = pod.spec.as_ref().and_then(|s| s.tolerations.clone()).unwrap_or_default();
        Some(taints.iter().filter(|t| t.effect == "PreferNoSchedule" && !tols.iter().any(|tol| tolerates(tol, t))).count() as i64)
    }
    fn normalize(&self) -> Normalize {
        Normalize::Reverse
    }
}

const MB: i64 = 1024 * 1024;
const MIN_THRESHOLD: i64 = 23 * MB;
const MAX_CONTAINER_THRESHOLD: i64 = 1000 * MB;

/// Score: prefers nodes that already have the pod's images.
pub struct ImageLocality;

fn normalize_image(name: &str) -> String {
    if name.contains('@') || name.rsplit('/').next().is_some_and(|l| l.contains(':')) {
        name.to_string()
    } else {
        format!("{name}:latest")
    }
}

impl Plugin for ImageLocality {
    fn name(&self) -> &'static str {
        IMAGE_LOCALITY
    }
    fn score(&self, _: &CycleState, pod: &Pod, node: &NodeInfo) -> Option<i64> {
        let images: Vec<String> = pod.spec.iter().flat_map(|s| s.containers.iter()).filter_map(|c| c.image.clone()).collect();
        if images.is_empty() {
            return Some(0);
        }
        let sum: i64 = images
            .iter()
            .filter_map(|i| {
                let n = normalize_image(i);
                node.images.get(&n).or_else(|| node.images.get(i)).or_else(|| node.images.get(&format!("docker.io/library/{n}")))
            })
            .sum();
        let max = MAX_CONTAINER_THRESHOLD * images.len() as i64;
        let clamped = sum.clamp(MIN_THRESHOLD, max);
        Some((clamped - MIN_THRESHOLD) * 100 / (max - MIN_THRESHOLD))
    }
}
