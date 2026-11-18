//! Port of `pkg/apis/core/v1/helper` (QoS class computation and friends).

use std::collections::BTreeMap;

use k8s_api::core::v1::{Container, PodSpec};
use k8s_apimachinery::quantity::parse_milli;
use serde_json::Value;

/// `GetPodQOS`: Guaranteed / Burstable / BestEffort.
pub fn qos_class(spec: &PodSpec) -> &'static str {
    let containers: Vec<&Container> = spec.containers.iter().chain(spec.init_containers.iter().flatten()).collect();
    let mut any_set = false;
    let mut guaranteed = true;
    for c in &containers {
        let res = c.resources.clone().unwrap_or_default();
        let limits = res.limits.unwrap_or_default();
        // Requests default to limits when unset.
        let mut requests = limits.clone();
        requests.extend(res.requests.unwrap_or_default());
        if !limits.is_empty() || !requests.is_empty() {
            any_set = true;
        }
        for r in ["cpu", "memory"] {
            let (Some(l), Some(q)) = (limits.get(r), requests.get(r)) else {
                guaranteed = false;
                continue;
            };
            if parse_milli(l).ok() != parse_milli(q).ok() {
                guaranteed = false;
            }
        }
    }
    if !any_set {
        "BestEffort"
    } else if guaranteed {
        "Guaranteed"
    } else {
        "Burstable"
    }
}

/// QoS from an untyped pod spec (defaults to BestEffort if it cannot be decoded).
pub fn qos_class_value(spec: &Value) -> &'static str {
    serde_json::from_value::<PodSpec>(spec.clone()).map(|s| qos_class(&s)).unwrap_or("BestEffort")
}

/// Sum of container requests (max with init containers), in milli-units per resource.
/// Used by the scheduler (`NodeResourcesFit`) and kubelet admission.
pub fn pod_requests(spec: &PodSpec) -> BTreeMap<String, i128> {
    let mut sum: BTreeMap<String, i128> = BTreeMap::new();
    for c in &spec.containers {
        for (k, v) in c.resources.as_ref().and_then(|r| r.requests.clone()).unwrap_or_default() {
            *sum.entry(k).or_default() += parse_milli(&v).unwrap_or(0);
        }
    }
    for c in spec.init_containers.iter().flatten() {
        for (k, v) in c.resources.as_ref().and_then(|r| r.requests.clone()).unwrap_or_default() {
            let q = parse_milli(&v).unwrap_or(0);
            let e = sum.entry(k).or_default();
            *e = (*e).max(q);
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(v: Value) -> PodSpec {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn qos() {
        assert_eq!(qos_class(&spec(json!({"containers": [{"name": "a"}]}))), "BestEffort");
        assert_eq!(
            qos_class(&spec(json!({"containers": [{"name": "a", "resources": {"limits": {"cpu": "1", "memory": "1Gi"}}}]}))),
            "Guaranteed"
        );
        assert_eq!(qos_class(&spec(json!({"containers": [{"name": "a", "resources": {"requests": {"cpu": "100m"}}}]}))), "Burstable");
    }

    #[test]
    fn requests() {
        let s = spec(json!({"containers": [
            {"name": "a", "resources": {"requests": {"cpu": "100m", "memory": "1Mi"}}},
            {"name": "b", "resources": {"requests": {"cpu": "200m"}}}]}));
        let r = pod_requests(&s);
        assert_eq!(r["cpu"], 300);
    }
}
