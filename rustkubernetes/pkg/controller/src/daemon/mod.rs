//! Port of `pkg/controller/daemon` (simplified): one pod per eligible node.
//!
//! A node is eligible when it matches the template's `nodeSelector` and the
//! pod tolerates its `NoSchedule`/`NoExecute` taints (DaemonSet pods get the
//! standard daemon tolerations automatically). Pods are pinned to their node
//! with a required node affinity on `metadata.name` and bound by the scheduler,
//! like upstream. RollingUpdate replaces outdated pods `maxUnavailable` at a time.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use k8s_api::apps::v1::{DaemonSet, DaemonSetStatus};
use k8s_api::core::v1::{Node, Pod, Toleration};
use k8s_api::IntOrString;
use k8s_client_go::informers::SharedInformerFactory;
use k8s_client_go::tools::cache::{split_key, Store};
use k8s_client_go::tools::record::{EventRecorder, NORMAL};
use k8s_client_go::util::workqueue::WorkQueue;
use k8s_client_go::{Api, Client};
use serde_json::{json, Value};

use crate::controller_utils::*;
use crate::ControllerContext;

const REVISION_LABEL: &str = "controller-revision-hash";

pub struct Controller {
    client: Client,
    store: Store<DaemonSet>,
    pod_store: Store<Pod>,
    node_store: Store<Node>,
    informers: Arc<SharedInformerFactory>,
    queue: Queue,
    recorder: EventRecorder,
    resync: Duration,
}

/// Tolerations every DaemonSet pod gets (`AddOrUpdateDaemonPodTolerations`).
fn daemon_tolerations(host_network: bool) -> Vec<Toleration> {
    let t = |key: &str, effect: &str| Toleration {
        key: Some(key.into()),
        operator: Some("Exists".into()),
        effect: Some(effect.into()),
        ..Default::default()
    };
    let mut v = vec![
        t("node.kubernetes.io/not-ready", "NoExecute"),
        t("node.kubernetes.io/unreachable", "NoExecute"),
        t("node.kubernetes.io/disk-pressure", "NoSchedule"),
        t("node.kubernetes.io/memory-pressure", "NoSchedule"),
        t("node.kubernetes.io/pid-pressure", "NoSchedule"),
        t("node.kubernetes.io/unschedulable", "NoSchedule"),
    ];
    if host_network {
        v.push(t("node.kubernetes.io/network-unavailable", "NoSchedule"));
    }
    v
}

/// The node a daemon pod is meant for (nodeName or the metadata.name affinity).
fn target_node(p: &Pod) -> Option<String> {
    let spec = p.spec.as_ref()?;
    if let Some(n) = spec.node_name.clone().filter(|n| !n.is_empty()) {
        return Some(n);
    }
    spec.affinity
        .as_ref()?
        .pointer("/nodeAffinity/requiredDuringSchedulingIgnoredDuringExecution/nodeSelectorTerms/0/matchFields/0/values/0")
        .and_then(Value::as_str)
        .map(String::from)
}

impl Controller {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let inf = ctx.informers.informer::<DaemonSet>();
        let pod_inf = ctx.informers.informer::<Pod>();
        let node_inf = ctx.informers.informer::<Node>();
        let c = Arc::new(Self {
            client: ctx.client.clone(),
            store: inf.store(),
            pod_store: pod_inf.store(),
            node_store: node_inf.store(),
            informers: ctx.informers.clone(),
            queue: WorkQueue::new(),
            recorder: ctx.recorder("daemonset-controller"),
            resync: ctx.config.resync_period,
        });
        let me = c.clone();
        on_events(inf.subscribe(), move |ev| me.queue.add(key(ev.object().as_ref())));
        let me = c.clone();
        on_events(pod_inf.subscribe(), move |ev| {
            if let Some(k) = owner_key(&ev.object().metadata, "DaemonSet") {
                me.queue.add(k);
            }
        });
        let me = c.clone();
        on_events(node_inf.subscribe(), move |_| me.store.keys().into_iter().for_each(|k| me.queue.add(k)));
        c
    }

    pub async fn run(self: Arc<Self>, workers: usize) {
        self.informers.wait_for_cache_sync().await;
        let store = self.store.clone();
        resync_every(self.queue.clone(), self.resync, move || store.keys());
        let me = self.clone();
        run_workers("daemonset-controller", self.queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.sync(&k).await }
        })
        .await;
    }

    fn should_run(&self, ds: &DaemonSet, node: &Node) -> bool {
        let tspec = ds.spec.as_ref().and_then(|s| s.template.spec.clone()).unwrap_or_default();
        let labels = node.metadata.labels();
        if let Some(sel) = &tspec.node_selector {
            if !sel.iter().all(|(k, v)| labels.get(k) == Some(v)) {
                return false;
            }
        }
        let mut tols = tspec.tolerations.clone().unwrap_or_default();
        tols.extend(daemon_tolerations(tspec.host_network.unwrap_or(false)));
        let taints = node.spec.as_ref().and_then(|s| s.taints.clone()).unwrap_or_default();
        untolerated(&taints, &tols, &["NoSchedule", "NoExecute"]).is_empty()
    }

    fn new_pod(&self, ds: &DaemonSet, node: &str, rev: &str) -> Pod {
        let spec = ds.spec.clone().unwrap_or_default();
        let mut pod =
            pod_from_template(&spec.template, ds.metadata.namespace(), &format!("{}-", ds.metadata.name()), Some(new_controller_ref(ds)));
        pod.metadata.labels.get_or_insert_with(BTreeMap::new).insert(REVISION_LABEL.into(), rev.into());
        let ps = pod.spec.get_or_insert_with(Default::default);
        ps.affinity = Some(json!({"nodeAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": {
            "nodeSelectorTerms": [{"matchFields": [{"key": "metadata.name", "operator": "In", "values": [node]}]}]}}}));
        let tols = ps.tolerations.get_or_insert_with(Vec::new);
        for t in daemon_tolerations(ps.host_network.unwrap_or(false)) {
            if !tols.contains(&t) {
                tols.push(t);
            }
        }
        pod
    }

    async fn sync(&self, key: &str) -> SyncResult {
        let (ns, _) = split_key(key);
        let Some(ds) = self.store.get(key) else { return Ok(None) };
        let spec = ds.spec.clone().unwrap_or_default();
        let api: Api<Pod> = Api::namespaced(self.client.clone(), ns);
        let owner = new_controller_ref(ds.as_ref());
        let selector = selector_of(spec.selector.as_ref());
        let deleting = ds.metadata.deletion_timestamp.is_some();
        let pods = claim_pods(&api, self.pod_store.list_namespace(ns), &owner, &selector, deleting).await?;
        let rev = compute_hash(&serde_json::to_value(&spec.template)?, None);
        let min_ready = spec.min_ready_seconds.unwrap_or(0);

        let mut by_node: BTreeMap<String, Vec<Arc<Pod>>> = BTreeMap::new();
        for p in pods.iter().filter(|p| p.metadata.deletion_timestamp.is_none()) {
            if let Some(n) = target_node(p) {
                by_node.entry(n).or_default().push(p.clone());
            }
        }
        let nodes = self.node_store.list();
        let mut desired = 0;
        let mut misscheduled = 0;
        if !deleting {
            for node in &nodes {
                let name = node.metadata.name().to_string();
                let on_node = by_node.get(&name).cloned().unwrap_or_default();
                if self.should_run(&ds, node) {
                    desired += 1;
                    match on_node.len() {
                        0 => {
                            let p = api.create(&self.new_pod(&ds, &name, &rev)).await?;
                            self.recorder.event(ds.as_ref(), NORMAL, "SuccessfulCreate", format!("Created pod: {}", p.metadata.name()));
                        }
                        _ => {
                            // Failed pods are recreated; duplicates beyond the oldest are removed.
                            let mut sorted = on_node.clone();
                            sorted.sort_by(|a, b| a.metadata.creation_timestamp.cmp(&b.metadata.creation_timestamp));
                            for (i, p) in sorted.iter().enumerate() {
                                if i > 0 || pod_phase(p) == "Failed" {
                                    delete_quiet(&api, p.metadata.name()).await?;
                                }
                            }
                        }
                    }
                } else if !on_node.is_empty() {
                    misscheduled += on_node.len() as i32;
                    for p in on_node {
                        delete_quiet(&api, p.metadata.name()).await?;
                    }
                }
            }
            // Pods for nodes that no longer exist.
            for (n, ps) in &by_node {
                if !nodes.iter().any(|x| x.metadata.name() == n) {
                    for p in ps {
                        delete_quiet(&api, p.metadata.name()).await?;
                    }
                }
            }
            // Rolling update.
            let us = spec.update_strategy.clone().unwrap_or_default();
            if us.get("type").and_then(Value::as_str) != Some("OnDelete") {
                let max_unavail: IntOrString =
                    serde_json::from_value(us.pointer("/rollingUpdate/maxUnavailable").cloned().unwrap_or(json!(1)))
                        .unwrap_or(IntOrString::Int(1));
                let max_unavail = max_unavail.scaled_value(desired as i64, true).unwrap_or(1).max(1);
                let current: Vec<&Arc<Pod>> = by_node.values().flatten().collect();
                let mut unavailable = current.iter().filter(|p| !is_pod_available(p, min_ready)).count() as i64;
                for p in current.iter().filter(|p| p.metadata.labels().get(REVISION_LABEL) != Some(&rev)) {
                    let avail = is_pod_available(p, min_ready);
                    if !avail || unavailable < max_unavail {
                        delete_quiet(&api, p.metadata.name()).await?;
                        if avail {
                            unavailable += 1;
                        }
                    }
                }
            }
        }

        let current: Vec<&Arc<Pod>> = by_node.values().flatten().collect();
        let mut st = ds.status.clone().unwrap_or_default();
        st = DaemonSetStatus {
            current_number_scheduled: current.len() as i32 - misscheduled,
            number_misscheduled: misscheduled,
            desired_number_scheduled: desired,
            number_ready: current.iter().filter(|p| is_pod_ready(p)).count() as i32,
            observed_generation: ds.metadata.generation,
            updated_number_scheduled: Some(current.iter().filter(|p| p.metadata.labels().get(REVISION_LABEL) == Some(&rev)).count() as i32),
            number_available: Some(current.iter().filter(|p| is_pod_available(p, min_ready)).count() as i32),
            number_unavailable: Some((desired - current.iter().filter(|p| is_pod_available(p, min_ready)).count() as i32).max(0)),
            collision_count: st.collision_count,
            conditions: st.conditions,
        };
        if ds.status.as_ref() != Some(&st) {
            let mut n = (*ds).clone();
            n.status = Some(st);
            let dapi: Api<DaemonSet> = Api::namespaced(self.client.clone(), ns);
            match dapi.replace_status(&n).await {
                Ok(_) => {}
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(None)
    }
}

async fn delete_quiet(api: &Api<Pod>, name: &str) -> anyhow::Result<()> {
    match api.delete(name).await {
        Ok(_) => Ok(()),
        Err(e) if e.is_not_found() => Ok(()),
        Err(e) => Err(e.into()),
    }
}
