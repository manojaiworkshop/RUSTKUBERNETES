//! The scheduling algorithm (`schedule_one.go`: `schedulePod`, `findNodesThatFitPod`,
//! `prioritizeNodes`, `selectHost`) and preemption (`framework/preemption`,
//! `plugins/defaultpreemption`).

use std::collections::BTreeMap;
use std::sync::Arc;

use k8s_api::core::v1::Pod;

use crate::apis::Profile;
use crate::framework::{normalize_scores, Code, CycleState, NodeInfo, Status};

/// Why a pod fits nowhere (`FitError` / `Diagnosis`): node name → status.
#[derive(Debug, Default)]
pub struct Diagnosis {
    pub node_status: BTreeMap<String, Status>,
    pub pre_filter: Option<Status>,
}

impl Diagnosis {
    /// `0/3 nodes are available: 1 Insufficient cpu, 2 node(s) had untolerated taint {...}.`
    pub fn message(&self, total: usize) -> String {
        if let Some(pf) = &self.pre_filter {
            return format!("0/{total} nodes are available: {}.", pf.reasons.join(", "));
        }
        if total == 0 {
            return "no nodes available to schedule pods".into();
        }
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for s in self.node_status.values() {
            for r in &s.reasons {
                *counts.entry(r.as_str()).or_default() += 1;
            }
        }
        let parts: Vec<String> = counts.iter().map(|(r, n)| format!("{n} {r}")).collect();
        format!("0/{total} nodes are available: {}.", parts.join(", "))
    }
}

pub enum Outcome {
    Scheduled(String),
    Unschedulable(Diagnosis),
}

/// Runs every filter plugin for one node (stops at the first failure).
pub fn run_filters(profile: &Profile, state: &CycleState, pod: &Pod, node: &NodeInfo) -> Status {
    for p in &profile.plugins {
        let s = p.filter(state, pod, node);
        if !s.is_success() {
            return s.with_plugin(p.name());
        }
    }
    Status::success()
}

pub fn run_pre_filters(profile: &Profile, state: &mut CycleState, pod: &Pod, nodes: &[NodeInfo]) -> Status {
    for p in &profile.plugins {
        let s = p.pre_filter(state, pod, nodes);
        if !s.is_success() {
            return s.with_plugin(p.name());
        }
    }
    Status::success()
}

/// Small xorshift RNG for tie-breaking (no external dependency).
fn rand_u64() -> u64 {
    use std::cell::Cell;
    thread_local!(static S: Cell<u64> = Cell::new(
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(42) | 1));
    S.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        x
    })
}

/// `schedulePod`: filter, score, select.
pub fn schedule_pod(profile: &Profile, pod: &Pod, nodes: &[NodeInfo]) -> Outcome {
    schedule_pod_with(profile, pod, nodes, CycleState::new())
}

/// `schedulePod` with a prepared cycle state (e.g. the volume binder).
pub fn schedule_pod_with(profile: &Profile, pod: &Pod, nodes: &[NodeInfo], mut state: CycleState) -> Outcome {
    let mut diag = Diagnosis::default();
    let pf = run_pre_filters(profile, &mut state, pod, nodes);
    if !pf.is_success() {
        diag.pre_filter = Some(pf);
        return Outcome::Unschedulable(diag);
    }
    let mut feasible: Vec<&NodeInfo> = vec![];
    for n in nodes {
        let s = run_filters(profile, &state, pod, n);
        if s.is_success() {
            feasible.push(n);
        } else {
            diag.node_status.insert(n.name().to_string(), s);
        }
    }
    match feasible.len() {
        0 => return Outcome::Unschedulable(diag),
        1 => return Outcome::Scheduled(feasible[0].name().to_string()),
        _ => {}
    }
    // Score.
    for (p, _) in &profile.scorers {
        p.pre_score(&mut state, pod, &feasible, nodes);
    }
    let mut totals = vec![0i64; feasible.len()];
    for (p, weight) in &profile.scorers {
        let mut raw: Vec<i64> = feasible.iter().map(|n| p.score(&state, pod, n).unwrap_or(0)).collect();
        normalize_scores(p.normalize(), &mut raw);
        for (t, s) in totals.iter_mut().zip(raw) {
            *t += s * weight;
        }
    }
    // selectHost: highest score, ties broken uniformly at random (reservoir sampling).
    let mut best = 0;
    let mut ties = 1u64;
    for i in 1..feasible.len() {
        if totals[i] > totals[best] {
            best = i;
            ties = 1;
        } else if totals[i] == totals[best] {
            ties += 1;
            if rand_u64().is_multiple_of(ties) {
                best = i;
            }
        }
    }
    Outcome::Scheduled(feasible[best].name().to_string())
}

fn priority(p: &Pod) -> i32 {
    p.spec.as_ref().and_then(|s| s.priority).unwrap_or(0)
}

/// A preemption plan: evict `victims` from `node`.
#[derive(Debug)]
pub struct Candidate {
    pub node: String,
    pub victims: Vec<Arc<Pod>>,
}

/// `DefaultPreemption.PostFilter`: find the node where evicting the fewest /
/// lowest-priority pods lets the preemptor fit.
pub fn find_preemption(profile: &Profile, pod: &Pod, nodes: &[NodeInfo], diag: &Diagnosis) -> Option<Candidate> {
    if pod.spec.as_ref().and_then(|s| s.preemption_policy.as_deref()) == Some("Never") || diag.pre_filter.is_some() {
        return None;
    }
    let prio = priority(pod);
    let mut candidates = vec![];
    for (i, n) in nodes.iter().enumerate() {
        // Preemption cannot help nodes rejected for unresolvable reasons.
        if diag.node_status.get(n.name()).is_none_or(|s| s.code != Code::Unschedulable) {
            continue;
        }
        let lower: Vec<Arc<Pod>> =
            n.pods.iter().filter(|p| priority(p) < prio && p.metadata.deletion_timestamp.is_none()).cloned().collect();
        if lower.is_empty() {
            continue;
        }
        // Remove every lower-priority pod and check the pod fits.
        let fits = |victims: &[Arc<Pod>]| {
            let mut snapshot = nodes.to_vec();
            for v in victims {
                snapshot[i].remove_pod(v.metadata.uid.as_deref().unwrap_or(""));
            }
            let mut st = CycleState::new();
            run_pre_filters(profile, &mut st, pod, &snapshot).is_success() && run_filters(profile, &st, pod, &snapshot[i]).is_success()
        };
        if !fits(&lower) {
            continue;
        }
        // Reprieve as many victims as possible, highest priority first.
        let mut sorted = lower.clone();
        sorted.sort_by_key(|p| std::cmp::Reverse(priority(p)));
        let mut victims = sorted.clone();
        for p in &sorted {
            let trial: Vec<Arc<Pod>> = victims.iter().filter(|v| v.metadata.uid != p.metadata.uid).cloned().collect();
            if fits(&trial) {
                victims = trial;
            }
        }
        candidates.push(Candidate { node: n.name().to_string(), victims });
    }
    // pickOneNodeForPreemption: lowest highest-victim priority, then lowest sum, then fewest victims.
    candidates.into_iter().min_by_key(|c| {
        let max = c.victims.iter().map(|p| priority(p)).max().unwrap_or(i32::MIN);
        let sum: i64 = c.victims.iter().map(|p| priority(p) as i64).sum();
        (max, sum, c.victims.len())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_api::core::v1::Node;
    use serde_json::json;

    fn node(name: &str, cpu: &str, labels: serde_json::Value, taints: serde_json::Value) -> NodeInfo {
        let n: Node = serde_json::from_value(json!({"metadata": {"name": name, "labels": labels},
            "spec": {"taints": taints}, "status": {"allocatable": {"cpu": cpu, "memory": "8Gi", "pods": "110"}}}))
        .unwrap();
        NodeInfo::new(Arc::new(n))
    }

    fn pod(name: &str, cpu: &str, prio: i32, extra: serde_json::Value) -> Pod {
        let mut v = json!({"metadata": {"name": name, "namespace": "d", "uid": name, "labels": {"app": name}},
            "spec": {"priority": prio, "containers": [{"name": "c", "image": "x", "resources": {"requests": {"cpu": cpu}}}]}});
        if let (Some(spec), Some(e)) = (v["spec"].as_object_mut(), extra.as_object()) {
            for (k, val) in e {
                spec.insert(k.clone(), val.clone());
            }
        }
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn filters_and_message() {
        let p = Profile::default_profile();
        let nodes = vec![
            node("small", "500m", json!({}), json!(null)),
            node("tainted", "8", json!({}), json!([{"key": "dedicated", "value": "gpu", "effect": "NoSchedule"}])),
            node("big", "8", json!({"disk": "ssd"}), json!(null)),
        ];
        match schedule_pod(&p, &pod("a", "1", 0, json!({})), &nodes) {
            Outcome::Scheduled(n) => assert_eq!(n, "big"),
            _ => panic!("should fit"),
        }
        match schedule_pod(&p, &pod("b", "1", 0, json!({"nodeSelector": {"disk": "hdd"}})), &nodes) {
            Outcome::Unschedulable(d) => {
                let m = d.message(3);
                assert!(m.starts_with("0/3 nodes are available:"), "{m}");
                assert!(m.contains("node(s) didn't match Pod's node affinity/selector"), "{m}");
            }
            _ => panic!("should not fit"),
        }
    }

    #[test]
    fn spreads_replicas_and_respects_anti_affinity() {
        let p = Profile::default_profile();
        let mut nodes = vec![
            node("n1", "8", json!({"kubernetes.io/hostname": "n1"}), json!(null)),
            node("n2", "8", json!({"kubernetes.io/hostname": "n2"}), json!(null)),
        ];
        let anti = json!({"affinity": {"podAntiAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": [
            {"labelSelector": {"matchLabels": {"app": "db"}}, "topologyKey": "kubernetes.io/hostname"}]}}});
        let mut db = pod("db", "100m", 0, anti.clone());
        db.metadata.labels = Some([("app".to_string(), "db".to_string())].into_iter().collect());
        nodes[0].add_pod(Arc::new(db.clone()));
        let mut db2 = db.clone();
        db2.metadata.name = Some("db2".into());
        db2.metadata.uid = Some("db2".into());
        match schedule_pod(&p, &db2, &nodes) {
            Outcome::Scheduled(n) => assert_eq!(n, "n2"),
            _ => panic!(),
        }
        nodes[1].add_pod(Arc::new(db2.clone()));
        let mut db3 = db;
        db3.metadata.name = Some("db3".into());
        db3.metadata.uid = Some("db3".into());
        assert!(matches!(schedule_pod(&p, &db3, &nodes), Outcome::Unschedulable(_)));
    }

    #[test]
    fn preemption_picks_lowest_priority_victims() {
        let p = Profile::default_profile();
        let mut nodes = vec![node("a", "2", json!({}), json!(null)), node("b", "2", json!({}), json!(null))];
        nodes[0].add_pod(Arc::new(pod("a-low", "1500m", 10, json!({}))));
        nodes[1].add_pod(Arc::new(pod("b-mid", "1500m", 50, json!({}))));
        let high = pod("high", "1", 100, json!({}));
        let Outcome::Unschedulable(d) = schedule_pod(&p, &high, &nodes) else { panic!() };
        let c = find_preemption(&p, &high, &nodes, &d).unwrap();
        assert_eq!(c.node, "a");
        assert_eq!(c.victims.len(), 1);
        assert_eq!(c.victims[0].metadata.name(), "a-low");
        // A pod with preemptionPolicy Never does not preempt.
        let never = pod("never", "1", 100, json!({"preemptionPolicy": "Never"}));
        assert!(find_preemption(&p, &never, &nodes, &d).is_none());
    }
}
