//! Port of `pkg/scheduler/framework/types.go`: `NodeInfo` and `Resource`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use k8s_api::core::v1::{Node, Pod};
use k8s_apimachinery::quantity::{parse_milli, parse_value};

/// Requested / allocatable resources (`framework.Resource`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resource {
    pub milli_cpu: i64,
    pub memory: i64,
    pub ephemeral_storage: i64,
    /// Number of pods (only meaningful for allocatable).
    pub pods: i64,
    /// Extended resources (`nvidia.com/gpu`, `hugepages-2Mi`, ...).
    pub scalar: BTreeMap<String, i64>,
}

impl Resource {
    /// From a resource list (`{"cpu": "500m", "memory": "1Gi"}`).
    pub fn from_list(list: &BTreeMap<String, String>) -> Self {
        let mut r = Resource::default();
        for (k, v) in list {
            match k.as_str() {
                "cpu" => r.milli_cpu = parse_milli(v).unwrap_or(0) as i64,
                "memory" => r.memory = parse_value(v).unwrap_or(0) as i64,
                "ephemeral-storage" => r.ephemeral_storage = parse_value(v).unwrap_or(0) as i64,
                "pods" => r.pods = parse_value(v).unwrap_or(0) as i64,
                other => {
                    r.scalar.insert(other.to_string(), parse_value(v).unwrap_or(0) as i64);
                }
            }
        }
        r
    }

    pub fn add(&mut self, o: &Resource) {
        self.milli_cpu += o.milli_cpu;
        self.memory += o.memory;
        self.ephemeral_storage += o.ephemeral_storage;
        for (k, v) in &o.scalar {
            *self.scalar.entry(k.clone()).or_default() += v;
        }
    }

    pub fn sub(&mut self, o: &Resource) {
        self.milli_cpu -= o.milli_cpu;
        self.memory -= o.memory;
        self.ephemeral_storage -= o.ephemeral_storage;
        for (k, v) in &o.scalar {
            *self.scalar.entry(k.clone()).or_default() -= v;
        }
    }

    fn max_with(&mut self, o: &Resource) {
        self.milli_cpu = self.milli_cpu.max(o.milli_cpu);
        self.memory = self.memory.max(o.memory);
        self.ephemeral_storage = self.ephemeral_storage.max(o.ephemeral_storage);
        for (k, v) in &o.scalar {
            let e = self.scalar.entry(k.clone()).or_default();
            *e = (*e).max(*v);
        }
    }
}

/// Default requests assumed for scoring when a container sets none
/// (`DefaultMilliCPURequest`, `DefaultMemoryRequest`).
pub const DEFAULT_MILLI_CPU_REQUEST: i64 = 100;
pub const DEFAULT_MEMORY_REQUEST: i64 = 200 * 1024 * 1024;

/// Effective pod requests (`resource.PodRequests`): sum of containers, at least
/// the largest init container (sidecars add up), plus `overhead`.
/// With `non_zero`, unset CPU/memory count as the scoring defaults.
pub fn pod_requests(pod: &Pod, non_zero: bool) -> Resource {
    let Some(spec) = pod.spec.as_ref() else { return Resource::default() };
    let req_of = |c: &k8s_api::core::v1::Container| {
        // Requests default to limits per resource (API defaulting).
        let res = c.resources.clone().unwrap_or_default();
        let mut list = res.limits.unwrap_or_default();
        list.extend(res.requests.unwrap_or_default());
        let mut r = Resource::from_list(&list);
        if non_zero {
            if !list.contains_key("cpu") {
                r.milli_cpu = DEFAULT_MILLI_CPU_REQUEST;
            }
            if !list.contains_key("memory") {
                r.memory = DEFAULT_MEMORY_REQUEST;
            }
        }
        r
    };
    let mut sum = Resource::default();
    for c in &spec.containers {
        sum.add(&req_of(c));
    }
    let mut sidecars = Resource::default();
    let mut init_max = Resource::default();
    for c in spec.init_containers.iter().flatten() {
        let r = req_of(c);
        if c.restart_policy.as_deref() == Some("Always") {
            sidecars.add(&r);
            sum.add(&r);
        } else {
            let mut with_sidecars = r.clone();
            with_sidecars.add(&sidecars);
            init_max.max_with(&with_sidecars);
        }
    }
    sum.max_with(&init_max);
    if let Some(Some(o)) = spec.extra.get("overhead").map(|v| serde_json::from_value::<BTreeMap<String, String>>(v.clone()).ok()) {
        sum.add(&Resource::from_list(&o));
    }
    sum
}

/// Aggregated per-node state (`framework.NodeInfo`).
#[derive(Clone, Debug)]
pub struct NodeInfo {
    pub node: Arc<Node>,
    pub pods: Vec<Arc<Pod>>,
    pub requested: Resource,
    pub non_zero_requested: Resource,
    pub allocatable: Resource,
    /// `(hostIP, protocol, hostPort)` in use.
    pub used_ports: Vec<(String, String, i32)>,
    /// image name → size in bytes.
    pub images: HashMap<String, i64>,
}

fn host_ports(p: &Pod) -> Vec<(String, String, i32)> {
    p.spec
        .iter()
        .flat_map(|s| s.containers.iter())
        .flat_map(|c| c.ports.iter().flatten())
        .filter_map(|cp| {
            let hp = cp.host_port.unwrap_or(0);
            (hp > 0)
                .then(|| (cp.host_ip.clone().unwrap_or_else(|| "0.0.0.0".into()), cp.protocol.clone().unwrap_or_else(|| "TCP".into()), hp))
        })
        .collect()
}

pub fn wanted_host_ports(p: &Pod) -> Vec<(String, String, i32)> {
    host_ports(p)
}

impl NodeInfo {
    pub fn new(node: Arc<Node>) -> Self {
        let alloc = node
            .status
            .as_ref()
            .and_then(|s| s.allocatable.clone().or_else(|| s.capacity.clone()))
            .map(|l| Resource::from_list(&l))
            .unwrap_or_default();
        let mut images = HashMap::new();
        for img in node.status.as_ref().and_then(|s| s.images.clone()).unwrap_or_default() {
            let size = img["sizeBytes"].as_i64().unwrap_or(0);
            for n in img["names"].as_array().into_iter().flatten().filter_map(|n| n.as_str()) {
                images.insert(n.to_string(), size);
            }
        }
        Self {
            node,
            pods: vec![],
            requested: Resource::default(),
            non_zero_requested: Resource::default(),
            allocatable: alloc,
            used_ports: vec![],
            images,
        }
    }

    pub fn name(&self) -> &str {
        self.node.metadata.name()
    }

    /// Replaces the node object, keeping pods.
    pub fn set_node(&mut self, node: Arc<Node>) {
        let pods = std::mem::take(&mut self.pods);
        *self = NodeInfo::new(node);
        for p in pods {
            self.add_pod(p);
        }
    }

    pub fn add_pod(&mut self, pod: Arc<Pod>) {
        self.requested.add(&pod_requests(&pod, false));
        self.non_zero_requested.add(&pod_requests(&pod, true));
        self.used_ports.extend(host_ports(&pod));
        self.pods.push(pod);
    }

    /// Removes a pod by UID; returns true if it was present.
    pub fn remove_pod(&mut self, uid: &str) -> bool {
        let Some(i) = self.pods.iter().position(|p| p.metadata.uid.as_deref() == Some(uid)) else { return false };
        let pod = self.pods.remove(i);
        self.requested.sub(&pod_requests(&pod, false));
        self.non_zero_requested.sub(&pod_requests(&pod, true));
        for hp in host_ports(&pod) {
            if let Some(j) = self.used_ports.iter().position(|x| *x == hp) {
                self.used_ports.remove(j);
            }
        }
        true
    }

    pub fn labels(&self) -> BTreeMap<String, String> {
        self.node.metadata.labels()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn requests_with_init_and_defaults() {
        let pod: Pod = serde_json::from_value(json!({"spec": {
            "containers": [{"name": "a", "resources": {"requests": {"cpu": "200m", "memory": "100Mi"}}},
                           {"name": "b", "resources": {"requests": {"cpu": "300m"}}}],
            "initContainers": [{"name": "i", "resources": {"requests": {"cpu": "1"}}}]}}))
        .unwrap();
        let r = pod_requests(&pod, false);
        assert_eq!(r.milli_cpu, 1000); // init container dominates
        assert_eq!(r.memory, 100 * 1024 * 1024);
        let nz = pod_requests(&pod, true);
        assert_eq!(nz.memory, 100 * 1024 * 1024 + DEFAULT_MEMORY_REQUEST);
    }
}
