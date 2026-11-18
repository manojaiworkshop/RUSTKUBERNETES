//! Port of `pkg/scheduler/backend/cache`: the scheduler's view of nodes and
//! the pods placed on them.
//!
//! **Assume**: right after choosing a node the pod is added to the cache, before
//! the binding is confirmed, so the next pod already sees the used resources.
//! When the informer reports the pod as bound, the assumption is confirmed; if
//! binding fails the pod is forgotten. Assumed pods whose binding finished but
//! never showed up expire after [`ASSUMED_TTL`].

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use k8s_api::core::v1::{Node, Pod};

use crate::framework::NodeInfo;

pub const ASSUMED_TTL: Duration = Duration::from_secs(30);

#[derive(Default)]
struct State {
    nodes: BTreeMap<String, NodeInfo>,
    /// uid → node name, for every pod in the cache.
    pod_node: HashMap<String, String>,
    /// Pods on nodes the cache does not know yet.
    pending: HashMap<String, Vec<Arc<Pod>>>,
    /// uid → expiry (None while binding is in flight).
    assumed: HashMap<String, Option<Instant>>,
}

#[derive(Default)]
pub struct Cache {
    st: Mutex<State>,
}

fn uid(p: &Pod) -> String {
    p.metadata.uid.clone().unwrap_or_default()
}

fn node_of(p: &Pod) -> Option<String> {
    p.spec.as_ref()?.node_name.clone().filter(|n| !n.is_empty())
}

impl State {
    fn insert_pod(&mut self, pod: Arc<Pod>) {
        let Some(n) = node_of(&pod) else { return };
        let id = uid(&pod);
        self.remove_pod(&id);
        match self.nodes.get_mut(&n) {
            Some(ni) => ni.add_pod(pod),
            None => self.pending.entry(n.clone()).or_default().push(pod),
        }
        self.pod_node.insert(id, n);
    }

    fn remove_pod(&mut self, id: &str) {
        if let Some(n) = self.pod_node.remove(id) {
            if let Some(ni) = self.nodes.get_mut(&n) {
                ni.remove_pod(id);
            }
            if let Some(v) = self.pending.get_mut(&n) {
                v.retain(|p| uid(p) != id);
            }
        }
    }
}

impl Cache {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn add_or_update_node(&self, node: Arc<Node>) {
        let mut st = self.st.lock().unwrap();
        let name = node.metadata.name().to_string();
        match st.nodes.get_mut(&name) {
            Some(ni) => ni.set_node(node),
            None => {
                let mut ni = NodeInfo::new(node);
                for p in st.pending.remove(&name).unwrap_or_default() {
                    ni.add_pod(p);
                }
                st.nodes.insert(name, ni);
            }
        }
    }

    pub fn remove_node(&self, name: &str) {
        let mut st = self.st.lock().unwrap();
        if let Some(ni) = st.nodes.remove(name) {
            // Keep the pods around in case the node comes back.
            st.pending.insert(name.to_string(), ni.pods);
        }
    }

    /// An assigned, non-terminated pod observed by the informer (confirms assumptions).
    pub fn add_or_update_pod(&self, pod: Arc<Pod>) {
        let mut st = self.st.lock().unwrap();
        st.assumed.remove(&uid(&pod));
        st.insert_pod(pod);
    }

    pub fn remove_pod(&self, pod: &Pod) {
        let mut st = self.st.lock().unwrap();
        let id = uid(pod);
        st.assumed.remove(&id);
        st.remove_pod(&id);
    }

    /// Adds the pod (with `spec.nodeName` set) before binding completes.
    pub fn assume_pod(&self, pod: Arc<Pod>) {
        let mut st = self.st.lock().unwrap();
        let id = uid(&pod);
        st.insert_pod(pod);
        st.assumed.insert(id, None);
    }

    pub fn finish_binding(&self, pod_uid: &str) {
        let mut st = self.st.lock().unwrap();
        if let Some(e) = st.assumed.get_mut(pod_uid) {
            *e = Some(Instant::now() + ASSUMED_TTL);
        }
    }

    pub fn forget_pod(&self, pod_uid: &str) {
        let mut st = self.st.lock().unwrap();
        if st.assumed.remove(pod_uid).is_some() {
            st.remove_pod(pod_uid);
        }
    }

    pub fn is_assumed(&self, pod_uid: &str) -> bool {
        self.st.lock().unwrap().assumed.contains_key(pod_uid)
    }

    /// Drops assumed pods whose binding finished long ago but never appeared.
    pub fn cleanup_expired(&self) {
        let mut st = self.st.lock().unwrap();
        let now = Instant::now();
        let expired: Vec<String> = st.assumed.iter().filter(|(_, e)| e.is_some_and(|t| t < now)).map(|(u, _)| u.clone()).collect();
        for u in expired {
            st.assumed.remove(&u);
            st.remove_pod(&u);
        }
    }

    /// A consistent copy of every known node (`UpdateSnapshot`).
    pub fn snapshot(&self) -> Vec<NodeInfo> {
        self.st.lock().unwrap().nodes.values().cloned().collect()
    }

    pub fn node_count(&self) -> usize {
        self.st.lock().unwrap().nodes.len()
    }
}
