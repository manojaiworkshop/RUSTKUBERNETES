//! `InterPodAffinity`: pod affinity and anti-affinity by topology domain.
//!
//! PreFilter builds, over the whole snapshot:
//! * topology pairs `(key, value)` forbidden by *existing* pods' required anti-affinity;
//! * per required affinity / anti-affinity term of the incoming pod, how many
//!   matching pods sit in each topology pair.
//!
//! Filter checks the node's labels against those maps; Score sums the weights
//! of preferred terms per topology pair.

use std::collections::{BTreeMap, HashMap, HashSet};

use k8s_api::core::v1::Pod;
use serde_json::Value;

use super::label_selector_matches;
use crate::framework::{CycleState, NodeInfo, Normalize, Plugin, Status};

pub const NAME: &str = "InterPodAffinity";
const STATE: &str = "PreFilterInterPodAffinity";
const SCORE_STATE: &str = "PreScoreInterPodAffinity";

type Pair = (String, String);

/// Does `term` (owned by a pod in `owner_ns`) select pod `p`?
pub fn term_selects(term: &Value, owner_ns: &str, p: &Pod) -> bool {
    let ns = p.metadata.namespace();
    let namespaces: Vec<&str> = term["namespaces"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let ns_ok = if !namespaces.is_empty() {
        namespaces.contains(&ns)
    } else if term.get("namespaceSelector").is_some_and(|s| !s.is_null()) {
        true // namespaceSelector: all namespaces (labels of namespaces are not in the snapshot)
    } else {
        ns == owner_ns
    };
    ns_ok && label_selector_matches(term.get("labelSelector"), &p.metadata.labels())
}

fn terms(pod: &Pod, kind: &str, required: bool) -> Vec<Value> {
    let aff = pod.spec.as_ref().and_then(|s| s.affinity.as_ref());
    let path = if required {
        format!("/{kind}/requiredDuringSchedulingIgnoredDuringExecution")
    } else {
        format!("/{kind}/preferredDuringSchedulingIgnoredDuringExecution")
    };
    aff.and_then(|a| a.pointer(&path)).and_then(Value::as_array).cloned().unwrap_or_default()
}

#[derive(Default)]
struct PreFilterState {
    existing_anti: HashSet<Pair>,
    affinity: Vec<(Value, HashMap<Pair, usize>)>,
    anti: Vec<(Value, HashMap<Pair, usize>)>,
}

fn pair(node: &BTreeMap<String, String>, key: &str) -> Option<Pair> {
    node.get(key).map(|v| (key.to_string(), v.clone()))
}

pub struct InterPodAffinity;

impl Plugin for InterPodAffinity {
    fn name(&self) -> &'static str {
        NAME
    }

    fn pre_filter(&self, state: &mut CycleState, pod: &Pod, nodes: &[NodeInfo]) -> Status {
        let mut st = PreFilterState::default();
        let aff_terms = terms(pod, "podAffinity", true);
        let anti_terms = terms(pod, "podAntiAffinity", true);
        st.affinity = aff_terms.into_iter().map(|t| (t, HashMap::new())).collect();
        st.anti = anti_terms.into_iter().map(|t| (t, HashMap::new())).collect();
        for n in nodes {
            let labels = n.labels();
            for e in &n.pods {
                // Existing pods' anti-affinity against the incoming pod (symmetry).
                for t in terms(e, "podAntiAffinity", true) {
                    if term_selects(&t, e.metadata.namespace(), pod) {
                        if let Some(p) = pair(&labels, t["topologyKey"].as_str().unwrap_or("")) {
                            st.existing_anti.insert(p);
                        }
                    }
                }
                for (t, counts) in st.affinity.iter_mut().chain(st.anti.iter_mut()) {
                    if term_selects(t, pod.metadata.namespace(), e) {
                        if let Some(p) = pair(&labels, t["topologyKey"].as_str().unwrap_or("")) {
                            *counts.entry(p).or_default() += 1;
                        }
                    }
                }
            }
        }
        state.write(STATE, st);
        Status::success()
    }

    fn filter(&self, state: &CycleState, pod: &Pod, node: &NodeInfo) -> Status {
        let Some(st) = state.read::<PreFilterState>(STATE) else { return Status::success() };
        let labels = node.labels();
        if st.existing_anti.iter().any(|(k, v)| labels.get(k) == Some(v)) {
            return Status::unschedulable("node(s) didn't satisfy existing pods anti-affinity rules");
        }
        for (t, counts) in &st.anti {
            if let Some(p) = pair(&labels, t["topologyKey"].as_str().unwrap_or("")) {
                if counts.get(&p).copied().unwrap_or(0) > 0 {
                    return Status::unschedulable("node(s) didn't match pod anti-affinity rules");
                }
            }
        }
        for (t, counts) in &st.affinity {
            let key = t["topologyKey"].as_str().unwrap_or("");
            let ok = match pair(&labels, key) {
                Some(p) => counts.get(&p).copied().unwrap_or(0) > 0,
                None => false,
            };
            // First pod of a group that selects itself may go anywhere with the key.
            let no_matches_anywhere = counts.is_empty();
            let self_match = term_selects(t, pod.metadata.namespace(), pod);
            let first_of_group = no_matches_anywhere && self_match && labels.contains_key(key);
            if !(ok || first_of_group) {
                return Status::unresolvable("node(s) didn't match pod affinity rules");
            }
        }
        Status::success()
    }

    fn pre_score(&self, state: &mut CycleState, pod: &Pod, _feasible: &[&NodeInfo], all: &[NodeInfo]) {
        let mut scores: HashMap<Pair, i64> = HashMap::new();
        let prefs: Vec<(Value, i64)> = terms(pod, "podAffinity", false)
            .into_iter()
            .map(|t| (t["podAffinityTerm"].clone(), t["weight"].as_i64().unwrap_or(0)))
            .chain(
                terms(pod, "podAntiAffinity", false)
                    .into_iter()
                    .map(|t| (t["podAffinityTerm"].clone(), -t["weight"].as_i64().unwrap_or(0))),
            )
            .collect();
        if prefs.is_empty() {
            return;
        }
        for n in all {
            let labels = n.labels();
            for e in &n.pods {
                for (t, w) in &prefs {
                    if term_selects(t, pod.metadata.namespace(), e) {
                        if let Some(p) = pair(&labels, t["topologyKey"].as_str().unwrap_or("")) {
                            *scores.entry(p).or_default() += w;
                        }
                    }
                }
            }
        }
        state.write(SCORE_STATE, scores);
    }

    fn score(&self, state: &CycleState, _pod: &Pod, node: &NodeInfo) -> Option<i64> {
        let Some(scores) = state.read::<HashMap<Pair, i64>>(SCORE_STATE) else { return Some(0) };
        let labels = node.labels();
        Some(scores.iter().filter(|((k, v), _)| labels.get(k) == Some(v)).map(|(_, s)| s).sum())
    }

    fn normalize(&self) -> Normalize {
        Normalize::MinMax
    }
}
