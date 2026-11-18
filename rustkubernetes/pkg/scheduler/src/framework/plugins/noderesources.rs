//! `NodeResourcesFit` and `NodeResourcesBalancedAllocation`.

use k8s_api::core::v1::Pod;
use serde_json::Value;

use crate::framework::{pod_requests, CycleState, NodeInfo, Plugin, Resource, Status};

pub const FIT: &str = "NodeResourcesFit";
pub const BALANCED_ALLOCATION: &str = "NodeResourcesBalancedAllocation";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScoringStrategy {
    LeastAllocated,
    MostAllocated,
}

/// Filter: the pod's requests must fit in `allocatable - requested`.
/// Score: `LeastAllocated` (spread, default) or `MostAllocated` (bin-pack).
pub struct Fit {
    pub strategy: ScoringStrategy,
}

impl Fit {
    /// `pluginConfig: {name: NodeResourcesFit, args: {scoringStrategy: {type: MostAllocated}}}`
    pub fn from_args(args: Option<&Value>) -> Self {
        let t = args.and_then(|a| a.pointer("/scoringStrategy/type")).and_then(Value::as_str);
        Self { strategy: if t == Some("MostAllocated") { ScoringStrategy::MostAllocated } else { ScoringStrategy::LeastAllocated } }
    }
}

/// Reasons the pod does not fit (`fitsRequest`).
pub fn insufficient(req: &Resource, node: &NodeInfo) -> Vec<String> {
    let mut out = vec![];
    let alloc = &node.allocatable;
    let used = &node.requested;
    if alloc.pods > 0 && node.pods.len() as i64 + 1 > alloc.pods {
        out.push("Too many pods".to_string());
    }
    if req.milli_cpu > 0 && req.milli_cpu > alloc.milli_cpu - used.milli_cpu {
        out.push("Insufficient cpu".to_string());
    }
    if req.memory > 0 && req.memory > alloc.memory - used.memory {
        out.push("Insufficient memory".to_string());
    }
    if req.ephemeral_storage > 0 && req.ephemeral_storage > alloc.ephemeral_storage - used.ephemeral_storage {
        out.push("Insufficient ephemeral-storage".to_string());
    }
    for (k, v) in &req.scalar {
        if *v > 0 && *v > alloc.scalar.get(k).copied().unwrap_or(0) - used.scalar.get(k).copied().unwrap_or(0) {
            out.push(format!("Insufficient {k}"));
        }
    }
    out
}

const STATE_KEY: &str = "PreFilterNodeResourcesFit";

impl Plugin for Fit {
    fn name(&self) -> &'static str {
        FIT
    }

    fn pre_filter(&self, state: &mut CycleState, pod: &Pod, _: &[NodeInfo]) -> Status {
        state.write(STATE_KEY, pod_requests(pod, false));
        Status::success()
    }

    fn filter(&self, state: &CycleState, pod: &Pod, node: &NodeInfo) -> Status {
        let req = state.read::<Resource>(STATE_KEY).cloned().unwrap_or_else(|| pod_requests(pod, false));
        let reasons = insufficient(&req, node);
        if reasons.is_empty() {
            Status::success()
        } else {
            Status { code: crate::framework::Code::Unschedulable, reasons, plugin: FIT.into() }
        }
    }

    fn score(&self, _: &CycleState, pod: &Pod, node: &NodeInfo) -> Option<i64> {
        let req = pod_requests(pod, true);
        let score_one = |requested: i64, alloc: i64| -> i64 {
            if alloc <= 0 {
                return 0;
            }
            let used = (requested).min(alloc);
            match self.strategy {
                ScoringStrategy::LeastAllocated => (alloc - used) * 100 / alloc,
                ScoringStrategy::MostAllocated => used * 100 / alloc,
            }
        };
        let cpu = score_one(node.non_zero_requested.milli_cpu + req.milli_cpu, node.allocatable.milli_cpu);
        let mem = score_one(node.non_zero_requested.memory + req.memory, node.allocatable.memory);
        Some((cpu + mem) / 2)
    }
}

/// Score: prefer nodes where CPU and memory utilisation stay balanced.
pub struct BalancedAllocation;

impl Plugin for BalancedAllocation {
    fn name(&self) -> &'static str {
        BALANCED_ALLOCATION
    }
    fn score(&self, _: &CycleState, pod: &Pod, node: &NodeInfo) -> Option<i64> {
        let req = pod_requests(pod, true);
        let frac = |r: i64, a: i64| if a <= 0 { 0.0 } else { ((r as f64) / (a as f64)).min(1.0) };
        let cpu = frac(node.non_zero_requested.milli_cpu + req.milli_cpu, node.allocatable.milli_cpu);
        let mem = frac(node.non_zero_requested.memory + req.memory, node.allocatable.memory);
        let mean = (cpu + mem) / 2.0;
        let std = (((cpu - mean).powi(2) + (mem - mean).powi(2)) / 2.0).sqrt();
        Some(((1.0 - std) * 100.0) as i64)
    }
}
