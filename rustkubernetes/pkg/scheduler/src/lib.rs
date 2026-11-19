//! Port of `k8s.io/kubernetes/pkg/scheduler`.
//!
//! | Module | Go equivalent |
//! |--------|---------------|
//! | [`framework`] | `framework` (Status, CycleState, NodeInfo, Plugin) + `framework/plugins` |
//! | [`backend::queue`] | `backend/queue` (activeQ / backoffQ / unschedulable) |
//! | [`backend::cache`] | `backend/cache` (assume / confirm / forget) |
//! | [`algorithm`] | `schedule_one.go` + `framework/preemption` |
//! | [`apis`] | `apis/config` (KubeSchedulerConfiguration, profiles) |
//!
//! [`Scheduler::run`] pops one pod at a time, runs the profile's plugins against
//! a snapshot of the cache, assumes the pod on the chosen node and binds it
//! asynchronously through `pods/binding`. Pods that fit nowhere get a
//! `PodScheduled=False` condition, a `FailedScheduling` event and — if allowed —
//! trigger preemption of lower-priority pods.

pub mod algorithm;
pub mod apis;
pub mod backend {
    pub mod cache;
    pub mod queue;
}
pub mod framework;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use k8s_api::core::v1::{Node, Pod};
use k8s_client_go::tools::cache::{Event, Informer};
use k8s_client_go::tools::record::{EventRecorder, NORMAL, WARNING};
use k8s_client_go::{Api, Client, ListParams, PatchType};
use serde_json::json;
use tokio::sync::broadcast;

use algorithm::{find_preemption, schedule_pod_with, Outcome};
use apis::Profile;
use backend::cache::Cache;
use backend::queue::{pod_key, QueuedPod, SchedulingQueue};
use framework::plugins::volumebinding::{self, VolumeBinder};

pub struct Scheduler {
    client: Client,
    pub queue: Arc<SchedulingQueue>,
    pub cache: Arc<Cache>,
    profiles: HashMap<String, Arc<Profile>>,
    recorder: EventRecorder,
    pod_inf: Arc<Informer<Pod>>,
    /// Pods that preempted others and wait for their nominated node
    /// (uid → (node, pod)); their resources are reserved on that node.
    nominated: std::sync::Mutex<HashMap<String, (String, Arc<Pod>)>>,
    node_inf: Arc<Informer<Node>>,
    /// Claims/volumes/classes for `VolumeBinding`.
    volume_binder: Arc<VolumeBinder>,
}

fn is_assigned(p: &Pod) -> bool {
    p.spec.as_ref().and_then(|s| s.node_name.as_deref()).is_some_and(|n| !n.is_empty())
}

fn is_terminal(p: &Pod) -> bool {
    matches!(p.status.as_ref().and_then(|s| s.phase.as_deref()), Some("Succeeded") | Some("Failed"))
}

fn scheduler_name(p: &Pod) -> &str {
    p.spec.as_ref().and_then(|s| s.scheduler_name.as_deref()).unwrap_or(apis::DEFAULT_SCHEDULER_NAME)
}

impl Scheduler {
    pub fn new(client: Client, profiles: Vec<Profile>) -> Arc<Self> {
        let profiles: HashMap<String, Arc<Profile>> = profiles.into_iter().map(|p| (p.scheduler_name.clone(), Arc::new(p))).collect();
        let gate_profiles = profiles.clone();
        let queue = SchedulingQueue::new(move |pod: &Pod| {
            gate_profiles.get(scheduler_name(pod)).is_none_or(|prof| prof.plugins.iter().all(|pl| pl.pre_enqueue(pod).is_success()))
        });
        let pod_inf = Informer::new(Api::<Pod>::all(client.clone()), ListParams::default());
        let node_inf = Informer::new(Api::<Node>::all(client.clone()), ListParams::default());
        let volume_binder = VolumeBinder::new(client.clone());
        Arc::new(Self {
            recorder: EventRecorder::new(client.clone(), "default-scheduler"),
            client,
            queue,
            cache: Cache::new(),
            profiles,
            pod_inf,
            node_inf,
            nominated: std::sync::Mutex::new(HashMap::new()),
            volume_binder,
        })
    }

    pub fn profile_names(&self) -> Vec<String> {
        self.profiles.keys().cloned().collect()
    }

    fn responsible(&self, p: &Pod) -> bool {
        self.profiles.contains_key(scheduler_name(p))
    }

    /// `addAllEventHandlers`
    /// Tracks `status.nominatedNodeName` of pending pods (`nominator`).
    fn track_nomination(&self, p: &Arc<Pod>, gone: bool) {
        let uid = p.metadata.uid.clone().unwrap_or_default();
        let node = p.status.as_ref().and_then(|s| s.nominated_node_name.clone()).filter(|n| !n.is_empty());
        let mut nom = self.nominated.lock().unwrap();
        match node {
            Some(n) if !gone && !is_assigned(p) => {
                nom.insert(uid, (n, p.clone()));
            }
            _ => {
                nom.remove(&uid);
            }
        }
    }

    /// Snapshot plus reservations: nominated pods with priority ≥ the pod being
    /// scheduled count as if already running on their nominated node.
    fn snapshot_for(&self, pod: &Pod) -> Vec<framework::NodeInfo> {
        let mut nodes = self.cache.snapshot();
        let prio = pod.spec.as_ref().and_then(|s| s.priority).unwrap_or(0);
        let nom = self.nominated.lock().unwrap();
        for (uid, (node, np)) in nom.iter() {
            if Some(uid) == pod.metadata.uid.as_ref() || np.spec.as_ref().and_then(|s| s.priority).unwrap_or(0) < prio {
                continue;
            }
            if let Some(ni) = nodes.iter_mut().find(|n| n.name() == node) {
                ni.add_pod(np.clone());
            }
        }
        nodes
    }

    fn handle_pod(&self, ev: Event<Pod>) {
        match &ev {
            Event::Deleted(p) => self.track_nomination(p, true),
            Event::Added(p) | Event::Updated { new: p, .. } => self.track_nomination(p, false),
        }
        match ev {
            Event::Added(p) | Event::Updated { new: p, .. } => {
                if is_assigned(&p) {
                    self.queue.delete(&pod_key(&p));
                    if is_terminal(&p) {
                        self.cache.remove_pod(&p);
                        self.queue.move_all("AssignedPodTerminated");
                    } else {
                        self.cache.add_or_update_pod(p);
                        self.queue.move_all("AssignedPodUpdate");
                    }
                } else if self.responsible(&p) && p.metadata.deletion_timestamp.is_none() && !is_terminal(&p) {
                    self.queue.add(p);
                } else {
                    self.queue.delete(&pod_key(&p));
                }
            }
            Event::Deleted(p) => {
                if is_assigned(&p) {
                    self.cache.remove_pod(&p);
                    self.queue.move_all("AssignedPodDelete");
                } else {
                    self.queue.delete(&pod_key(&p));
                }
            }
        }
    }

    fn handle_node(&self, ev: Event<Node>) {
        match ev {
            Event::Added(n) | Event::Updated { new: n, .. } => {
                self.cache.add_or_update_node(n);
                self.queue.move_all("NodeUpdate");
            }
            Event::Deleted(n) => self.cache.remove_node(n.metadata.name()),
        }
    }

    fn spawn_handler<K: Send + Sync + 'static>(self: &Arc<Self>, mut rx: broadcast::Receiver<Event<K>>, f: fn(&Self, Event<K>)) {
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => f(&me, ev),
                    Err(broadcast::error::RecvError::Lagged(n)) => tracing::warn!("scheduler lagged {n} events"),
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    /// Starts informers and the scheduling loop; runs forever.
    pub async fn run(self: Arc<Self>) {
        self.spawn_handler(self.pod_inf.subscribe(), Scheduler::handle_pod);
        self.spawn_handler(self.node_inf.subscribe(), Scheduler::handle_node);
        // Claim, volume and class changes may make waiting pods schedulable (queueing hints).
        self.spawn_handler(self.volume_binder.claims.subscribe(), |s, _| s.queue.move_all("PvcChange"));
        self.spawn_handler(self.volume_binder.volumes.subscribe(), |s, _| s.queue.move_all("PvChange"));
        self.spawn_handler(self.volume_binder.classes.subscribe(), |s, _| s.queue.move_all("StorageClassChange"));
        self.node_inf.start();
        self.pod_inf.start();
        self.volume_binder.start().await;
        self.node_inf.wait_synced().await;
        self.pod_inf.wait_synced().await;
        tracing::info!(profiles = ?self.profile_names(), nodes = self.cache.node_count(), "scheduler caches synced");
        let cache = self.cache.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_secs(1));
            loop {
                t.tick().await;
                cache.cleanup_expired();
            }
        });
        while let Some(qp) = self.queue.pop().await {
            self.schedule_one(qp).await;
        }
    }

    /// `ScheduleOne`
    pub async fn schedule_one(self: &Arc<Self>, qp: QueuedPod) {
        let pod = qp.pod.clone();
        let key = qp.key();
        let uid = pod.metadata.uid.clone().unwrap_or_default();
        if pod.metadata.deletion_timestamp.is_some() || is_assigned(&pod) || self.cache.is_assumed(&uid) {
            self.queue.done(&key);
            return;
        }
        let Some(profile) = self.profiles.get(scheduler_name(&pod)).cloned() else {
            self.queue.done(&key);
            return;
        };
        let nodes = self.snapshot_for(&pod);
        let started = std::time::Instant::now();
        let mut state = framework::CycleState::new();
        state.write(volumebinding::BINDER_KEY, self.volume_binder.clone());
        match schedule_pod_with(&profile, &pod, &nodes, state) {
            Outcome::Scheduled(node) => {
                let mut assumed = (*pod).clone();
                assumed.spec.get_or_insert_with(Default::default).node_name = Some(node.clone());
                self.cache.assume_pod(Arc::new(assumed));
                tracing::debug!(pod = %key, node = %node, elapsed = ?started.elapsed(), "scheduled");
                let me = self.clone();
                let binds_volumes = profile.plugins.iter().any(|p| p.name() == volumebinding::NAME);
                let labels = nodes.iter().find(|n| n.name() == node).map(|n| n.labels()).unwrap_or_default();
                tokio::spawn(async move { me.bind(qp, node, binds_volumes, labels).await });
            }
            Outcome::Unschedulable(diag) => {
                let mut msg = diag.message(nodes.len());
                let waiting_for_victims = self.waiting_for_victims(&pod, &nodes);
                if waiting_for_victims {
                    msg.push_str(" preemption: waiting for preempted pods to terminate.");
                } else if let Some(c) = find_preemption(&profile, &pod, &nodes, &diag) {
                    msg.push_str(&format!(" preemption: preempting {} pod(s) on node {}.", c.victims.len(), c.node));
                    self.preempt(&pod, &c).await;
                } else if !nodes.is_empty() && diag.pre_filter.is_none() {
                    msg.push_str(&format!(
                        " preemption: 0/{} nodes are available: {} Preemption is not helpful for scheduling.",
                        nodes.len(),
                        nodes.len()
                    ));
                }
                tracing::info!(pod = %key, "unschedulable: {msg}");
                self.record_failure(&pod, "Unschedulable", &msg).await;
                self.queue.add_unschedulable(qp);
            }
        }
    }

    /// `PodEligibleToPreemptOthers`: a nominated pod waits while lower-priority
    /// pods on its nominated node are still terminating.
    fn waiting_for_victims(&self, pod: &Pod, nodes: &[framework::NodeInfo]) -> bool {
        let Some(nominated) = pod.status.as_ref().and_then(|s| s.nominated_node_name.as_deref()).filter(|n| !n.is_empty()) else {
            return false;
        };
        let prio = pod.spec.as_ref().and_then(|s| s.priority).unwrap_or(0);
        nodes
            .iter()
            .filter(|n| n.name() == nominated)
            .flat_map(|n| n.pods.iter())
            .any(|p| p.metadata.deletion_timestamp.is_some() && p.spec.as_ref().and_then(|s| s.priority).unwrap_or(0) < prio)
    }

    /// `bindingCycle` with the DefaultBinder: `POST pods/<name>/binding`.
    async fn bind(
        self: Arc<Self>,
        qp: QueuedPod,
        node: String,
        binds_volumes: bool,
        node_labels: std::collections::BTreeMap<String, String>,
    ) {
        let pod = qp.pod.clone();
        let uid = pod.metadata.uid.clone().unwrap_or_default();
        // PreBind: VolumeBinding.
        if binds_volumes {
            if let Err(e) = self.volume_binder.bind_pod_volumes(&pod, &node, &node_labels).await {
                self.cache.forget_pod(&uid);
                tracing::warn!(pod = %qp.key(), "PreBind failed: {e}");
                self.record_failure(&pod, "SchedulerError", &format!("running PreBind plugin \"VolumeBinding\": {e}")).await;
                self.queue.add_backoff(qp);
                return;
            }
        }
        let api: Api<Pod> = Api::namespaced(self.client.clone(), pod.metadata.namespace());
        let binding = json!({
            "apiVersion": "v1", "kind": "Binding",
            "metadata": {"name": pod.metadata.name(), "namespace": pod.metadata.namespace(), "uid": uid},
            "target": {"apiVersion": "v1", "kind": "Node", "name": node},
        });
        match api.dynamic().create_subresource(pod.metadata.name(), "binding", &binding).await {
            Ok(_) => {
                self.cache.finish_binding(&uid);
                self.queue.done(&qp.key());
                self.recorder.event(
                    pod.as_ref(),
                    NORMAL,
                    "Scheduled",
                    format!("Successfully assigned {}/{} to {node}", pod.metadata.namespace(), pod.metadata.name()),
                );
            }
            Err(e) => {
                self.cache.forget_pod(&uid);
                if e.is_not_found() {
                    self.queue.delete(&qp.key());
                    return;
                }
                tracing::warn!(pod = %qp.key(), "binding failed: {e}");
                self.record_failure(&pod, "SchedulerError", &format!("Binding rejected: {e}")).await;
                self.queue.add_backoff(qp);
            }
        }
    }

    /// Evicts victims and nominates the node for the preemptor.
    async fn preempt(&self, pod: &Pod, c: &algorithm::Candidate) {
        for v in &c.victims {
            let api: Api<Pod> = Api::namespaced(self.client.clone(), v.metadata.namespace());
            match api.delete(v.metadata.name()).await {
                Ok(_) => self.recorder.event(
                    v.as_ref(),
                    NORMAL,
                    "Preempted",
                    format!("Preempted by pod {} on node {}", pod.metadata.uid.as_deref().unwrap_or(""), c.node),
                ),
                Err(e) if e.is_not_found() => {}
                Err(e) => tracing::warn!("failed to preempt {}: {e}", v.metadata.name()),
            }
        }
        let api: Api<Pod> = Api::namespaced(self.client.clone(), pod.metadata.namespace());
        let mut nominated = pod.clone();
        nominated.status.get_or_insert_with(Default::default).nominated_node_name = Some(c.node.clone());
        self.track_nomination(&Arc::new(nominated), false);
        let _ = api.patch_status(pod.metadata.name(), PatchType::Merge, &json!({"status": {"nominatedNodeName": c.node}})).await;
    }

    /// Sets `PodScheduled=False` (if changed) and records `FailedScheduling`.
    async fn record_failure(&self, pod: &Pod, reason: &str, msg: &str) {
        self.recorder.event(pod, WARNING, "FailedScheduling", msg.to_string());
        let current = pod.status.as_ref().and_then(|s| s.conditions.as_ref()).and_then(|cs| cs.iter().find(|c| c.type_ == "PodScheduled"));
        if current.is_some_and(|c| c.status == "False" && c.message.as_deref() == Some(msg)) {
            return;
        }
        let api: Api<Pod> = Api::namespaced(self.client.clone(), pod.metadata.namespace());
        let patch = json!({"status": {"conditions": [{
            "type": "PodScheduled", "status": "False", "reason": reason, "message": msg,
            "lastProbeTime": null, "lastTransitionTime": k8s_apimachinery::meta::now_rfc3339(),
        }]}});
        if let Err(e) = api.patch_status(pod.metadata.name(), PatchType::Strategic, &patch).await {
            tracing::debug!("failed to update pod condition: {e}");
        }
    }
}
