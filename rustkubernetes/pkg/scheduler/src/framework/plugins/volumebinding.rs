//! `VolumeBinding` (`framework/plugins/volumebinding`).
//!
//! PreFilter: every claim the pod uses must exist and not be deleting; unbound
//! claims of an `Immediate` class make the pod wait ("pod has unbound immediate
//! PersistentVolumeClaims"). Filter: bound volumes must fit the node's
//! (`nodeAffinity`); claims of a `WaitForFirstConsumer` class need a matching
//! Available volume on the node, or a class that can provision. PreBind (in the
//! binding cycle): bind those claims for the chosen node — a matched volume gets
//! its `claimRef`, a claim to provision gets `volume.kubernetes.io/selected-node`
//! — and wait until the PV controller has finished binding them.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use k8s_api::core::v1::{PersistentVolume, PersistentVolumeClaim, Pod};
use k8s_api::storage::v1::StorageClass;
use k8s_client_go::tools::cache::{Informer, Store};
use k8s_client_go::{Api, Client, ListParams};
use k8s_component_helpers::scheduling::nodeaffinity::requirement_matches;
use k8s_component_helpers::storage::volume::*;

use crate::framework::{CycleState, NodeInfo, Plugin, Status};

pub const NAME: &str = "VolumeBinding";
/// CycleState key of the scheduler's [`VolumeBinder`].
pub const BINDER_KEY: &str = "VolumeBinding/binder";
const STATE_KEY: &str = "PreFilterVolumeBinding";
/// `bindTimeoutSeconds` default.
pub const BIND_TIMEOUT: Duration = Duration::from_secs(600);

pub const ERR_UNBOUND_IMMEDIATE: &str = "pod has unbound immediate PersistentVolumeClaims";
pub const ERR_BIND_CONFLICT: &str = "node(s) didn't find available persistent volumes to bind";
pub const ERR_NODE_CONFLICT: &str = "node(s) had volume node affinity conflict";
pub const ERR_PV_NOT_EXIST: &str = "node(s) unavailable due to one or more pvc(s) bound to non-existent pv(s)";

/// Claims, volumes and classes the scheduler watches (the volume binder's listers).
pub struct VolumeBinder {
    client: Client,
    pub claims: Arc<Informer<PersistentVolumeClaim>>,
    pub volumes: Arc<Informer<PersistentVolume>>,
    pub classes: Arc<Informer<StorageClass>>,
}

/// For each delayed claim: the static volume to bind, or `None` to provision.
type Matches = Vec<(Arc<PersistentVolumeClaim>, Option<PersistentVolume>)>;

#[derive(Clone, Default)]
struct PodClaims {
    bound: Vec<Arc<PersistentVolumeClaim>>,
    /// Unbound claims of `WaitForFirstConsumer` classes.
    delayed: Vec<Arc<PersistentVolumeClaim>>,
}

impl VolumeBinder {
    pub fn new(client: Client) -> Arc<Self> {
        Arc::new(Self {
            claims: Informer::new(Api::<PersistentVolumeClaim>::all(client.clone()), ListParams::default()),
            volumes: Informer::new(Api::<PersistentVolume>::all(client.clone()), ListParams::default()),
            classes: Informer::new(Api::<k8s_api::storage::v1::StorageClass>::all(client.clone()), ListParams::default()),
            client,
        })
    }

    pub async fn start(&self) {
        self.claims.start();
        self.volumes.start();
        self.classes.start();
        self.claims.wait_synced().await;
        self.volumes.wait_synced().await;
        self.classes.wait_synced().await;
    }

    fn claim_store(&self) -> Store<PersistentVolumeClaim> {
        self.claims.store()
    }

    fn class(&self, name: &str) -> Option<Arc<StorageClass>> {
        (!name.is_empty()).then(|| self.classes.store().get(name)).flatten()
    }

    fn delayed(&self, c: &PersistentVolumeClaim) -> bool {
        self.class(&claim_class(c)).is_some_and(|s| s.wait_for_first_consumer())
    }

    /// `GetPodVolumeClaims`
    fn pod_claims(&self, pod: &Pod) -> Result<PodClaims, Status> {
        let ns = pod.metadata.namespace();
        let mut out = PodClaims::default();
        for name in pod_claim_names(pod) {
            let Some(c) = self.claim_store().get_ns(ns, &name) else {
                return Err(Status::unresolvable(format!("persistentvolumeclaim \"{name}\" not found")));
            };
            if c.metadata.deletion_timestamp.is_some() {
                return Err(Status::unresolvable(format!("persistentvolumeclaim \"{name}\" is being deleted")));
            }
            let bound =
                !claim_volume_name(&c).is_empty() && c.metadata.annotations.as_ref().is_some_and(|a| a.contains_key(ANN_BIND_COMPLETED));
            if bound {
                out.bound.push(c);
            } else if self.delayed(&c) {
                out.delayed.push(c);
            } else {
                return Err(Status::unresolvable(ERR_UNBOUND_IMMEDIATE));
            }
        }
        Ok(out)
    }

    /// `checkVolumeProvisions`: can the class provision on this node?
    fn can_provision(&self, c: &PersistentVolumeClaim, labels: &BTreeMap<String, String>) -> bool {
        let Some(class) = self.class(&claim_class(c)) else { return false };
        if class.provisioner == NOT_SUPPORTED_PROVISIONER {
            return false;
        }
        // `allowedTopologies`: one term with all its label expressions satisfied.
        match class.allowed_topologies.as_ref().filter(|t| !t.is_empty()) {
            None => true,
            Some(terms) => terms.iter().any(|t| {
                t["matchLabelExpressions"].as_array().into_iter().flatten().all(|e| {
                    let r = serde_json::json!({"key": e["key"], "operator": "In", "values": e["values"]});
                    requirement_matches(&r, labels)
                })
            }),
        }
    }

    /// `FindPodVolumes` for one node: `Ok(matches)` with the static volume (or `None`
    /// to provision) for every delayed claim, or the reasons the node does not fit.
    fn find(&self, claims: &PodClaims, node_name: &str, labels: &BTreeMap<String, String>) -> Result<Matches, Vec<&'static str>> {
        let mut reasons = vec![];
        let volumes = self.volumes.store();
        for c in &claims.bound {
            match volumes.get(claim_volume_name(c)) {
                None => reasons.push(ERR_PV_NOT_EXIST),
                Some(v) => {
                    if check_node_affinity(&v, labels, node_name).is_err() {
                        reasons.push(ERR_NODE_CONFLICT);
                    }
                }
            }
        }
        let all = volumes.list();
        let refs: Vec<&PersistentVolume> = all.iter().map(|v| v.as_ref()).collect();
        let node = NodeRef { name: node_name, labels };
        let mut chosen = HashSet::new();
        let mut out = vec![];
        for c in &claims.delayed {
            match find_matching_volume(c, &refs, Some(&node), &chosen, true) {
                Some(v) => {
                    chosen.insert(v.metadata.name().to_string());
                    out.push((c.clone(), Some(v.clone())));
                }
                None if self.can_provision(c, labels) => out.push((c.clone(), None)),
                None => {
                    if !reasons.contains(&ERR_BIND_CONFLICT) {
                        reasons.push(ERR_BIND_CONFLICT);
                    }
                }
            }
        }
        if reasons.is_empty() {
            Ok(out)
        } else {
            Err(reasons)
        }
    }

    /// PreBind (`BindPodVolumes`): API updates for the chosen node, then wait for the
    /// PV controller to complete every binding.
    pub async fn bind_pod_volumes(&self, pod: &Pod, node: &str, node_labels: &BTreeMap<String, String>) -> Result<(), String> {
        let claims = self.pod_claims(pod).map_err(|s| s.reasons.join(", "))?;
        if claims.delayed.is_empty() {
            return Ok(());
        }
        let matches = self.find(&claims, node, node_labels).map_err(|r| r.join(", "))?;
        let pv_api: Api<PersistentVolume> = Api::all(self.client.clone());
        let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(self.client.clone(), pod.metadata.namespace());
        for (c, v) in &matches {
            match v {
                Some(v) => {
                    let (bound, dirty) = bind_volume_to_claim(v, c);
                    if dirty {
                        pv_api.replace(&bound).await.map_err(|e| format!("binding volumes: {e}"))?;
                    }
                }
                None => {
                    let mut c2 = (**c).clone();
                    let a = c2.metadata.annotations.get_or_insert_with(Default::default);
                    if a.get(ANN_SELECTED_NODE).map(String::as_str) != Some(node) {
                        a.insert(ANN_SELECTED_NODE.into(), node.into());
                        pvc_api.replace(&c2).await.map_err(|e| format!("binding volumes: {e}"))?;
                    }
                }
            }
        }
        // `checkBindings`, polled until done or `bindTimeoutSeconds`.
        let deadline = tokio::time::Instant::now() + BIND_TIMEOUT;
        loop {
            let mut done = true;
            for (c, _) in &matches {
                let Some(cur) = pvc_api.get_opt(c.metadata.name()).await.map_err(|e| format!("binding volumes: {e}"))? else {
                    return Err(format!("binding volumes: pvc {:?} was deleted", c.metadata.name()));
                };
                let bound = !claim_volume_name(&cur).is_empty()
                    && cur.metadata.annotations.as_ref().is_some_and(|a| a.contains_key(ANN_BIND_COMPLETED));
                if !bound {
                    done = false;
                }
            }
            if done {
                return Ok(());
            }
            if tokio::time::Instant::now() > deadline {
                return Err("binding volumes: context deadline exceeded".into());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

pub struct VolumeBinding;

impl Plugin for VolumeBinding {
    fn name(&self) -> &'static str {
        NAME
    }

    fn pre_filter(&self, state: &mut CycleState, pod: &Pod, _nodes: &[NodeInfo]) -> Status {
        let Some(b) = state.read::<Arc<VolumeBinder>>(BINDER_KEY).cloned() else { return Status::success() };
        match b.pod_claims(pod) {
            Ok(claims) => {
                state.write(STATE_KEY, claims);
                Status::success()
            }
            Err(s) => s,
        }
    }

    fn filter(&self, state: &CycleState, _pod: &Pod, node: &NodeInfo) -> Status {
        let (Some(b), Some(claims)) = (state.read::<Arc<VolumeBinder>>(BINDER_KEY), state.read::<PodClaims>(STATE_KEY)) else {
            return Status::success();
        };
        if claims.bound.is_empty() && claims.delayed.is_empty() {
            return Status::success();
        }
        match b.find(claims, node.name(), &node.labels()) {
            Ok(_) => Status::success(),
            Err(reasons) => {
                let mut s = Status::unresolvable(reasons[0]);
                s.reasons = reasons.iter().map(|r| r.to_string()).collect();
                s
            }
        }
    }
}
