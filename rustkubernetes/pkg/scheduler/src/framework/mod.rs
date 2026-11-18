//! Port of `pkg/scheduler/framework`: the scheduling framework.
//!
//! A [`Plugin`] can implement any of the extension points used in one
//! scheduling cycle:
//!
//! ```text
//! PreEnqueue → (queue) → PreFilter → Filter (per node) → [PostFilter = preemption]
//!            → PreScore → Score (per node) → NormalizeScore → select host
//!            → assume → bind (DefaultBinder)
//! ```
//!
//! Plugins read cluster state only from the snapshot ([`NodeInfo`]s) and pass
//! precomputed data from PreFilter/PreScore to Filter/Score via [`CycleState`].

pub mod plugins;
pub mod types;

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use k8s_api::core::v1::Pod;

pub use types::{pod_requests, NodeInfo, Resource};

/// `framework.Code`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    Success,
    /// Not schedulable now; preemption may help.
    Unschedulable,
    /// Not schedulable and preemption cannot help (e.g. node selector mismatch).
    UnschedulableAndUnresolvable,
    Error,
}

/// `framework.Status`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub code: Code,
    pub reasons: Vec<String>,
    pub plugin: String,
}

impl Status {
    pub fn success() -> Self {
        Self { code: Code::Success, reasons: vec![], plugin: String::new() }
    }
    pub fn unschedulable(reason: impl Into<String>) -> Self {
        Self { code: Code::Unschedulable, reasons: vec![reason.into()], plugin: String::new() }
    }
    pub fn unresolvable(reason: impl Into<String>) -> Self {
        Self { code: Code::UnschedulableAndUnresolvable, reasons: vec![reason.into()], plugin: String::new() }
    }
    pub fn error(msg: impl Into<String>) -> Self {
        Self { code: Code::Error, reasons: vec![msg.into()], plugin: String::new() }
    }
    pub fn is_success(&self) -> bool {
        self.code == Code::Success
    }
    pub fn with_plugin(mut self, p: &str) -> Self {
        self.plugin = p.into();
        self
    }
}

/// Per-cycle scratch space shared between a plugin's extension points (`CycleState`).
/// Values are reference counted, so cloning a state (for preemption simulation) is cheap.
#[derive(Clone, Default)]
pub struct CycleState {
    data: HashMap<&'static str, Arc<dyn Any + Send + Sync>>,
}

impl CycleState {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn write<T: Any + Send + Sync>(&mut self, key: &'static str, v: T) {
        self.data.insert(key, Arc::new(v));
    }
    pub fn read<T: Any + Send + Sync>(&self, key: &'static str) -> Option<&T> {
        self.data.get(key)?.downcast_ref::<T>()
    }
}

/// How raw scores of a plugin are mapped to `0..=100` (`ScoreExtensions.NormalizeScore`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Normalize {
    /// Scores are already 0..=100.
    None,
    /// `score * 100 / max`
    Max,
    /// `100 - score * 100 / max` (lower raw score is better)
    Reverse,
    /// `(score - min) * 100 / (max - min)`
    MinMax,
}

/// One scheduler plugin. Every extension point has a no-op default.
pub trait Plugin: Send + Sync {
    fn name(&self) -> &'static str;

    /// Gate before the pod enters the active queue.
    fn pre_enqueue(&self, _pod: &Pod) -> Status {
        Status::success()
    }

    /// Precomputation over the whole snapshot; may reject the pod outright.
    fn pre_filter(&self, _state: &mut CycleState, _pod: &Pod, _nodes: &[NodeInfo]) -> Status {
        Status::success()
    }

    /// Can the pod run on this node?
    fn filter(&self, _state: &CycleState, _pod: &Pod, _node: &NodeInfo) -> Status {
        Status::success()
    }

    /// Precomputation for scoring over the feasible nodes.
    fn pre_score(&self, _state: &mut CycleState, _pod: &Pod, _feasible: &[&NodeInfo], _all: &[NodeInfo]) {}

    /// Raw score for a feasible node; `None` if this plugin does not score.
    fn score(&self, _state: &CycleState, _pod: &Pod, _node: &NodeInfo) -> Option<i64> {
        None
    }

    fn normalize(&self) -> Normalize {
        Normalize::None
    }
}

pub const MAX_NODE_SCORE: i64 = 100;

/// Applies a normalization to raw scores in place.
pub fn normalize_scores(kind: Normalize, scores: &mut [i64]) {
    let max = scores.iter().copied().max().unwrap_or(0);
    let min = scores.iter().copied().min().unwrap_or(0);
    for s in scores.iter_mut() {
        *s = match kind {
            Normalize::None => (*s).clamp(0, MAX_NODE_SCORE),
            Normalize::Max => {
                if max == 0 {
                    0
                } else {
                    *s * MAX_NODE_SCORE / max
                }
            }
            Normalize::Reverse => {
                if max == 0 {
                    MAX_NODE_SCORE
                } else {
                    MAX_NODE_SCORE - *s * MAX_NODE_SCORE / max
                }
            }
            Normalize::MinMax => {
                if max == min {
                    0
                } else {
                    (*s - min) * MAX_NODE_SCORE / (max - min)
                }
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization() {
        let mut s = vec![0, 5, 10];
        normalize_scores(Normalize::Max, &mut s);
        assert_eq!(s, [0, 50, 100]);
        let mut s = vec![0, 5, 10];
        normalize_scores(Normalize::Reverse, &mut s);
        assert_eq!(s, [100, 50, 0]);
        let mut s = vec![-4, 0, 4];
        normalize_scores(Normalize::MinMax, &mut s);
        assert_eq!(s, [0, 50, 100]);
    }

    #[test]
    fn cycle_state() {
        let mut st = CycleState::new();
        st.write("k", 5u32);
        let c = st.clone();
        assert_eq!(c.read::<u32>("k"), Some(&5));
        assert_eq!(c.read::<i32>("k"), None);
    }
}
