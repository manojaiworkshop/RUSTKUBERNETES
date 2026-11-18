//! Port of `pkg/controller/nodelifecycle` and `pkg/controller/tainteviction`.
//!
//! **Node lifecycle** (every `--node-monitor-period`): a node whose last
//! heartbeat (Lease `renewTime` in `kube-node-lease`, or its Ready condition's
//! `lastHeartbeatTime`) is older than `--node-monitor-grace-period` gets all
//! conditions set to `Unknown` and its pods marked not ready. Taints mirror
//! node health:
//!
//! | Ready condition | NoExecute taint | NoSchedule taint |
//! |-----------------|-----------------|------------------|
//! | `False` | `node.kubernetes.io/not-ready` | `node.kubernetes.io/not-ready` |
//! | `Unknown` | `node.kubernetes.io/unreachable` | `node.kubernetes.io/unreachable` |
//! | `spec.unschedulable` | — | `node.kubernetes.io/unschedulable` |
//!
//! **Taint eviction**: pods on a node with `NoExecute` taints they do not
//! tolerate are deleted; tolerations with `tolerationSeconds` delay the
//! eviction until `taint.timeAdded + tolerationSeconds`.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use k8s_api::coordination::v1::Lease;
use k8s_api::core::v1::{Node, NodeCondition, Pod, PodCondition, Taint};
use k8s_apimachinery::meta::{now_rfc3339, parse_time};
use k8s_client_go::informers::SharedInformerFactory;
use k8s_client_go::tools::cache::{split_key, Event, Store};
use k8s_client_go::tools::record::{EventRecorder, NORMAL};
use k8s_client_go::util::workqueue::WorkQueue;
use k8s_client_go::{Api, Client};

use crate::controller_utils::*;
use crate::ControllerContext;

pub const TAINT_NOT_READY: &str = "node.kubernetes.io/not-ready";
pub const TAINT_UNREACHABLE: &str = "node.kubernetes.io/unreachable";
pub const TAINT_UNSCHEDULABLE: &str = "node.kubernetes.io/unschedulable";
const MANAGED: &[&str] = &[TAINT_NOT_READY, TAINT_UNREACHABLE, TAINT_UNSCHEDULABLE];
const NODE_LEASE_NAMESPACE: &str = "kube-node-lease";

pub struct Controller {
    client: Client,
    node_store: Store<Node>,
    lease_store: Store<Lease>,
    pod_store: Store<Pod>,
    informers: Arc<SharedInformerFactory>,
    queue: Queue,
    recorder: EventRecorder,
    grace: Duration,
    period: Duration,
}

fn ready_condition(n: &Node) -> Option<&NodeCondition> {
    n.status.as_ref()?.conditions.as_ref()?.iter().find(|c| c.type_ == "Ready")
}

impl Controller {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let node_inf = ctx.informers.informer::<Node>();
        let lease_inf = ctx.informers.informer::<Lease>();
        let pod_inf = ctx.informers.informer::<Pod>();
        let c = Arc::new(Self {
            client: ctx.client.clone(),
            node_store: node_inf.store(),
            lease_store: lease_inf.store(),
            pod_store: pod_inf.store(),
            informers: ctx.informers.clone(),
            queue: WorkQueue::new(),
            recorder: ctx.recorder("node-controller"),
            grace: ctx.config.node_monitor_grace_period,
            period: ctx.config.node_monitor_period,
        });
        let me = c.clone();
        on_events(node_inf.subscribe(), move |ev| me.queue.add(key(ev.object().as_ref())));
        c
    }

    pub async fn run(self: Arc<Self>, workers: usize) {
        self.informers.wait_for_cache_sync().await;
        let store = self.node_store.clone();
        resync_every(self.queue.clone(), self.period, move || store.keys());
        let me = self.clone();
        run_workers("node-lifecycle-controller", self.queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.sync(&k).await }
        })
        .await;
    }

    fn last_heartbeat(&self, node: &Node) -> Option<chrono::DateTime<Utc>> {
        let from_lease = self
            .lease_store
            .get_ns(NODE_LEASE_NAMESPACE, node.metadata.name())
            .and_then(|l| l.spec.as_ref().and_then(|s| s.renew_time.clone()))
            .as_deref()
            .and_then(parse_time);
        let from_status = ready_condition(node).and_then(|c| c.last_heartbeat_time.as_deref()).and_then(parse_time);
        let created = node.metadata.creation_timestamp.as_deref().and_then(parse_time);
        [from_lease, from_status, created].into_iter().flatten().max()
    }

    async fn sync(&self, name: &str) -> SyncResult {
        let Some(node) = self.node_store.get(name) else { return Ok(None) };
        let api: Api<Node> = Api::all(self.client.clone());
        let mut node = (*node).clone();
        let stale = self.last_heartbeat(&node).is_none_or(|t| Utc::now() - t > chrono::Duration::from_std(self.grace).unwrap());
        let ready_status = ready_condition(&node).map(|c| c.status.clone()).unwrap_or_else(|| "Unknown".into());

        // 1. Heartbeat timeout → every condition Unknown.
        if stale && ready_status != "Unknown" {
            let now = now_rfc3339();
            let st = node.status.get_or_insert_with(Default::default);
            let conds = st.conditions.get_or_insert_with(Vec::new);
            for t in ["Ready", "MemoryPressure", "DiskPressure", "PIDPressure"] {
                let msg = "Kubelet stopped posting node status.".to_string();
                match conds.iter_mut().find(|c| c.type_ == t) {
                    Some(c) => {
                        c.status = "Unknown".into();
                        c.reason = Some("NodeStatusUnknown".into());
                        c.message = Some(msg);
                        c.last_transition_time = Some(now.clone());
                    }
                    None => conds.push(NodeCondition {
                        type_: t.into(),
                        status: "Unknown".into(),
                        reason: Some("NodeStatusNeverUpdated".into()),
                        message: Some("Kubelet never posted node status.".into()),
                        last_heartbeat_time: None,
                        last_transition_time: Some(now.clone()),
                    }),
                }
            }
            node = api.replace_status(&node).await?;
            self.recorder.event(&node, NORMAL, "NodeNotReady", format!("Node {name} status is now: NodeNotReady"));
            self.mark_pods_not_ready(name).await?;
        }

        // 2. Taints mirror the Ready condition.
        let ready_status = ready_condition(&node).map(|c| c.status.clone()).unwrap_or_else(|| "Unknown".into());
        let unschedulable = node.spec.as_ref().and_then(|s| s.unschedulable).unwrap_or(false);
        let mut desired: Vec<(&str, &str)> = vec![];
        match ready_status.as_str() {
            "False" => desired.extend([(TAINT_NOT_READY, "NoExecute"), (TAINT_NOT_READY, "NoSchedule")]),
            "Unknown" => desired.extend([(TAINT_UNREACHABLE, "NoExecute"), (TAINT_UNREACHABLE, "NoSchedule")]),
            _ => {}
        }
        if unschedulable {
            desired.push((TAINT_UNSCHEDULABLE, "NoSchedule"));
        }
        let current = node.spec.as_ref().and_then(|s| s.taints.clone()).unwrap_or_default();
        let have: BTreeSet<(String, String)> =
            current.iter().filter(|t| MANAGED.contains(&t.key.as_str())).map(|t| (t.key.clone(), t.effect.clone())).collect();
        let want: BTreeSet<(String, String)> = desired.iter().map(|(k, e)| (k.to_string(), e.to_string())).collect();
        if have != want {
            let now = now_rfc3339();
            let mut taints: Vec<Taint> = current
                .into_iter()
                .filter(|t| !MANAGED.contains(&t.key.as_str()) || want.contains(&(t.key.clone(), t.effect.clone())))
                .collect();
            for (k, e) in &want {
                if !have.contains(&(k.clone(), e.clone())) {
                    taints.push(Taint {
                        key: k.clone(),
                        value: None,
                        effect: e.clone(),
                        time_added: (e == "NoExecute").then(|| now.clone()),
                    });
                }
            }
            node.spec.get_or_insert_with(Default::default).taints = Some(taints).filter(|t| !t.is_empty());
            match api.replace(&node).await {
                Ok(_) => {}
                Err(e) if e.is_conflict() => return Ok(Some(Duration::from_millis(200))),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(None)
    }

    /// `MarkPodsNotReady`: sets the Ready condition of every pod on the node to False.
    async fn mark_pods_not_ready(&self, node: &str) -> anyhow::Result<()> {
        for p in self.pod_store.list().into_iter().filter(|p| p.spec.as_ref().and_then(|s| s.node_name.as_deref()) == Some(node)) {
            if !is_pod_ready(&p) {
                continue;
            }
            let mut p = (*p).clone();
            let conds = p.status.get_or_insert_with(Default::default).conditions.get_or_insert_with(Vec::new);
            let now = now_rfc3339();
            match conds.iter_mut().find(|c| c.type_ == "Ready") {
                Some(c) => {
                    c.status = "False".into();
                    c.last_transition_time = Some(now);
                }
                None => conds.push(PodCondition {
                    type_: "Ready".into(),
                    status: "False".into(),
                    last_transition_time: Some(now),
                    ..Default::default()
                }),
            }
            let api: Api<Pod> = Api::namespaced(self.client.clone(), p.metadata.namespace());
            match api.replace_status(&p).await {
                Ok(_) => {}
                Err(e) if e.is_not_found() || e.is_conflict() => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Taint-based eviction
// ---------------------------------------------------------------------------

pub struct TaintEvictionController {
    client: Client,
    node_store: Store<Node>,
    pod_store: Store<Pod>,
    informers: Arc<SharedInformerFactory>,
    queue: Queue,
    recorder: EventRecorder,
    resync: Duration,
}

impl TaintEvictionController {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let node_inf = ctx.informers.informer::<Node>();
        let pod_inf = ctx.informers.informer::<Pod>();
        let c = Arc::new(Self {
            client: ctx.client.clone(),
            node_store: node_inf.store(),
            pod_store: pod_inf.store(),
            informers: ctx.informers.clone(),
            queue: WorkQueue::new(),
            recorder: ctx.recorder("taint-eviction-controller"),
            resync: ctx.config.resync_period,
        });
        let me = c.clone();
        on_events(pod_inf.subscribe(), move |ev| {
            if !matches!(ev, Event::Deleted(_)) && ev.object().spec.as_ref().and_then(|s| s.node_name.as_ref()).is_some() {
                me.queue.add(key(ev.object().as_ref()));
            }
        });
        let me = c.clone();
        on_events(node_inf.subscribe(), move |ev| {
            let node = ev.object().metadata.name().to_string();
            for p in me.pod_store.list() {
                if p.spec.as_ref().and_then(|s| s.node_name.as_deref()) == Some(node.as_str()) {
                    me.queue.add(key(p.as_ref()));
                }
            }
        });
        c
    }

    pub async fn run(self: Arc<Self>) {
        self.informers.wait_for_cache_sync().await;
        let store = self.pod_store.clone();
        resync_every(self.queue.clone(), self.resync, move || store.keys());
        let me = self.clone();
        run_workers("taint-eviction-controller", self.queue.clone(), 2, move |k| {
            let me = me.clone();
            async move { me.sync(&k).await }
        })
        .await;
    }

    async fn sync(&self, key: &str) -> SyncResult {
        let (ns, name) = split_key(key);
        let Some(pod) = self.pod_store.get(key) else { return Ok(None) };
        if pod.metadata.deletion_timestamp.is_some() {
            return Ok(None);
        }
        let Some(node_name) = pod.spec.as_ref().and_then(|s| s.node_name.clone()) else { return Ok(None) };
        let Some(node) = self.node_store.get(&node_name) else { return Ok(None) };
        let taints: Vec<Taint> =
            node.spec.as_ref().and_then(|s| s.taints.clone()).unwrap_or_default().into_iter().filter(|t| t.effect == "NoExecute").collect();
        if taints.is_empty() {
            return Ok(None);
        }
        let tolerations = pod.spec.as_ref().and_then(|s| s.tolerations.clone()).unwrap_or_default();
        // Earliest moment any NoExecute taint stops being tolerated.
        let mut evict_at: Option<chrono::DateTime<Utc>> = None;
        for t in &taints {
            let matching: Vec<_> = tolerations.iter().filter(|tol| tolerates(tol, t)).collect();
            let deadline = if matching.is_empty() {
                Some(Utc::now())
            } else if matching.iter().any(|tol| tol.toleration_seconds.is_none()) {
                None // tolerated forever
            } else {
                let secs = matching.iter().filter_map(|tol| tol.toleration_seconds).min().unwrap_or(0).max(0);
                let added = t.time_added.as_deref().and_then(parse_time).unwrap_or_else(Utc::now);
                Some(added + chrono::Duration::seconds(secs))
            };
            if let Some(d) = deadline {
                evict_at = Some(evict_at.map_or(d, |e| e.min(d)));
            }
        }
        let Some(at) = evict_at else { return Ok(None) };
        let now = Utc::now();
        if at > now {
            return Ok(Some((at - now).to_std().unwrap_or_default()));
        }
        let api: Api<Pod> = Api::namespaced(self.client.clone(), ns);
        match api.delete(name).await {
            Ok(_) => self.recorder.event(pod.as_ref(), NORMAL, "TaintManagerEviction", format!("Marking for deletion Pod {key}")),
            Err(e) if e.is_not_found() => {}
            Err(e) => return Err(e.into()),
        }
        Ok(None)
    }
}
