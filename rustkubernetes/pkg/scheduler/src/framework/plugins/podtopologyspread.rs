//! `PodTopologySpread`: spread matching pods evenly across topology domains.
//!
//! * `whenUnsatisfiable: DoNotSchedule` constraints are filters: placing the pod
//!   in a domain must not make `count(domain) - min(count) > maxSkew`.
//! * `ScheduleAnyway` constraints only score (fewer matching pods is better).
//! * Pods without constraints that belong to a controller get the system
//!   default `ScheduleAnyway` constraints on `kubernetes.io/hostname` and
//!   `topology.kubernetes.io/zone`, so replicas spread across nodes.
//!
//! Only nodes passing the pod's node affinity/selector count as domains
//! (`nodeAffinityPolicy: Honor`).

use std::collections::BTreeMap;

use k8s_api::core::v1::Pod;
use serde_json::{json, Value};

use super::label_selector_matches;
use super::nodeaffinity::required_matches;
use crate::framework::{CycleState, NodeInfo, Normalize, Plugin, Status};

pub const NAME: &str = "PodTopologySpread";
const STATE: &str = "PreFilterPodTopologySpread";
const SCORE_STATE: &str = "PreScorePodTopologySpread";

#[derive(Clone, Debug)]
struct Constraint {
    max_skew: i64,
    key: String,
    hard: bool,
    selector: Value,
    min_domains: i64,
}

fn constraints(pod: &Pod) -> Vec<Constraint> {
    let raw = pod.spec.as_ref().and_then(|s| s.topology_spread_constraints.clone()).unwrap_or_default();
    let mut out: Vec<Constraint> = raw
        .iter()
        .map(|c| Constraint {
            max_skew: c["maxSkew"].as_i64().unwrap_or(1).max(1),
            key: c["topologyKey"].as_str().unwrap_or("").to_string(),
            hard: c["whenUnsatisfiable"].as_str().unwrap_or("DoNotSchedule") == "DoNotSchedule",
            selector: c.get("labelSelector").cloned().unwrap_or(Value::Null),
            min_domains: c["minDomains"].as_i64().unwrap_or(1).max(1),
        })
        .collect();
    if out.is_empty() && pod.metadata.controller_ref().is_some() {
        // System defaults: spread the pod's siblings (same labels) softly.
        let labels = pod.metadata.labels();
        let sel = json!({"matchLabels": labels});
        out.push(Constraint { max_skew: 3, key: "kubernetes.io/hostname".into(), hard: false, selector: sel.clone(), min_domains: 1 });
        out.push(Constraint { max_skew: 5, key: "topology.kubernetes.io/zone".into(), hard: false, selector: sel, min_domains: 1 });
    }
    out
}

/// Matching pods per domain value, for one constraint, over eligible nodes.
fn counts(c: &Constraint, pod: &Pod, nodes: &[NodeInfo]) -> BTreeMap<String, i64> {
    let mut m = BTreeMap::new();
    for n in nodes {
        let Some(v) = n.labels().get(&c.key).cloned() else { continue };
        if !required_matches(pod, n) {
            continue;
        }
        let k = n
            .pods
            .iter()
            .filter(|e| e.metadata.namespace() == pod.metadata.namespace() && e.metadata.deletion_timestamp.is_none())
            .filter(|e| label_selector_matches(Some(&c.selector), &e.metadata.labels()))
            .count() as i64;
        *m.entry(v).or_default() += k;
    }
    m
}

pub struct PodTopologySpread;

impl Plugin for PodTopologySpread {
    fn name(&self) -> &'static str {
        NAME
    }

    fn pre_filter(&self, state: &mut CycleState, pod: &Pod, nodes: &[NodeInfo]) -> Status {
        let hard: Vec<(Constraint, BTreeMap<String, i64>)> =
            constraints(pod).into_iter().filter(|c| c.hard).map(|c| (c.clone(), counts(&c, pod, nodes))).collect();
        state.write(STATE, hard);
        Status::success()
    }

    fn filter(&self, state: &CycleState, pod: &Pod, node: &NodeInfo) -> Status {
        let Some(hard) = state.read::<Vec<(Constraint, BTreeMap<String, i64>)>>(STATE) else { return Status::success() };
        let labels = node.labels();
        for (c, counts) in hard {
            let Some(v) = labels.get(&c.key) else {
                return Status::unresolvable("node(s) didn't match pod topology spread constraints (missing required label)");
            };
            let self_match = label_selector_matches(Some(&c.selector), &pod.metadata.labels()) as i64;
            // Fewer domains than minDomains: the global minimum is treated as 0.
            let min = if (counts.len() as i64) < c.min_domains { 0 } else { counts.values().copied().min().unwrap_or(0) };
            let here = counts.get(v).copied().unwrap_or(0);
            if here + self_match - min > c.max_skew {
                return Status::unschedulable("node(s) didn't match pod topology spread constraints");
            }
        }
        Status::success()
    }

    fn pre_score(&self, state: &mut CycleState, pod: &Pod, _feasible: &[&NodeInfo], all: &[NodeInfo]) {
        let soft: Vec<(Constraint, BTreeMap<String, i64>)> =
            constraints(pod).into_iter().filter(|c| !c.hard).map(|c| (c.clone(), counts(&c, pod, all))).collect();
        state.write(SCORE_STATE, soft);
    }

    fn score(&self, state: &CycleState, _pod: &Pod, node: &NodeInfo) -> Option<i64> {
        let soft = state.read::<Vec<(Constraint, BTreeMap<String, i64>)>>(SCORE_STATE)?;
        let labels = node.labels();
        Some(
            soft.iter()
                .map(|(c, counts)| labels.get(&c.key).and_then(|v| counts.get(v)).copied().unwrap_or(0) * (10 / c.max_skew.max(1)))
                .sum(),
        )
    }

    fn normalize(&self) -> Normalize {
        Normalize::Reverse
    }
}
