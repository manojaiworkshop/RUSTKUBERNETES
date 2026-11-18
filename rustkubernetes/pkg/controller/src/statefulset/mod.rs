//! Port of `pkg/controller/statefulset` (simplified): stable, ordered pods
//! `<name>-0 … <name>-(N-1)`.
//!
//! * `OrderedReady`: pod *i* is created only when pods 0..i are Running and
//!   Ready; scale-down removes the highest ordinal first, one at a time.
//! * `Parallel`: create/delete all missing/extra pods at once.
//! * `RollingUpdate`: pods whose `controller-revision-hash` differs from the
//!   update revision are deleted from the highest ordinal down to `partition`,
//!   one at a time, and recreated from the new template.
//!
//! Not yet: `volumeClaimTemplates` (no PVC API yet), ControllerRevision objects.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use k8s_api::apps::v1::{StatefulSet, StatefulSetStatus};
use k8s_api::core::v1::Pod;
use k8s_client_go::informers::SharedInformerFactory;
use k8s_client_go::tools::cache::{split_key, Store};
use k8s_client_go::tools::record::{EventRecorder, NORMAL};
use k8s_client_go::util::workqueue::WorkQueue;
use k8s_client_go::{Api, Client};
use serde_json::Value;

use crate::controller_utils::*;
use crate::ControllerContext;

pub const REVISION_LABEL: &str = "controller-revision-hash";
pub const POD_NAME_LABEL: &str = "statefulset.kubernetes.io/pod-name";
pub const POD_INDEX_LABEL: &str = "apps.kubernetes.io/pod-index";

pub struct Controller {
    client: Client,
    store: Store<StatefulSet>,
    pod_store: Store<Pod>,
    informers: Arc<SharedInformerFactory>,
    queue: Queue,
    recorder: EventRecorder,
    resync: Duration,
}

fn ordinal(sts: &str, pod: &str) -> Option<i32> {
    pod.strip_prefix(sts)?.strip_prefix('-')?.parse().ok()
}

fn running_and_ready(p: &Pod) -> bool {
    pod_phase(p) == "Running" && is_pod_ready(p) && p.metadata.deletion_timestamp.is_none()
}

fn revision(p: &Pod) -> Option<&String> {
    p.metadata.labels.as_ref()?.get(REVISION_LABEL)
}

impl Controller {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let inf = ctx.informers.informer::<StatefulSet>();
        let pod_inf = ctx.informers.informer::<Pod>();
        let c = Arc::new(Self {
            client: ctx.client.clone(),
            store: inf.store(),
            pod_store: pod_inf.store(),
            informers: ctx.informers.clone(),
            queue: WorkQueue::new(),
            recorder: ctx.recorder("statefulset-controller"),
            resync: ctx.config.resync_period,
        });
        let me = c.clone();
        on_events(inf.subscribe(), move |ev| me.queue.add(key(ev.object().as_ref())));
        let me = c.clone();
        on_events(pod_inf.subscribe(), move |ev| {
            if let Some(k) = owner_key(&ev.object().metadata, "StatefulSet") {
                me.queue.add(k);
            }
        });
        c
    }

    pub async fn run(self: Arc<Self>, workers: usize) {
        self.informers.wait_for_cache_sync().await;
        let store = self.store.clone();
        resync_every(self.queue.clone(), self.resync, move || store.keys());
        let me = self.clone();
        run_workers("statefulset-controller", self.queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.sync(&k).await }
        })
        .await;
    }

    fn new_pod(&self, sts: &StatefulSet, i: i32, rev: &str) -> Pod {
        let spec = sts.spec.clone().unwrap_or_default();
        let name = format!("{}-{i}", sts.metadata.name());
        let mut pod = pod_from_template(&spec.template, sts.metadata.namespace(), "", Some(new_controller_ref(sts)));
        pod.metadata.generate_name = None;
        pod.metadata.name = Some(name.clone());
        let labels = pod.metadata.labels.get_or_insert_with(BTreeMap::new);
        labels.insert(POD_NAME_LABEL.into(), name.clone());
        labels.insert(POD_INDEX_LABEL.into(), i.to_string());
        labels.insert(REVISION_LABEL.into(), rev.into());
        let ps = pod.spec.get_or_insert_with(Default::default);
        ps.hostname = Some(name);
        if let Some(svc) = spec.service_name.filter(|s| !s.is_empty()) {
            ps.subdomain = Some(svc);
        }
        pod
    }

    async fn sync(&self, key: &str) -> SyncResult {
        let (ns, _) = split_key(key);
        let Some(sts) = self.store.get(key) else { return Ok(None) };
        let spec = sts.spec.clone().unwrap_or_default();
        let api: Api<Pod> = Api::namespaced(self.client.clone(), ns);
        let owner = new_controller_ref(sts.as_ref());
        let selector = selector_of(spec.selector.as_ref());
        let deleting = sts.metadata.deletion_timestamp.is_some();
        let all = self.pod_store.list_namespace(ns);
        let pods = claim_pods(&api, all, &owner, &selector, deleting).await?;

        let replicas = spec.replicas.unwrap_or(1).max(0);
        let parallel = spec.pod_management_policy.as_deref() == Some("Parallel");
        let update_rev = format!("{}-{}", sts.metadata.name(), compute_hash(&serde_json::to_value(&spec.template)?, None));
        let mut by_ord: BTreeMap<i32, Arc<Pod>> = BTreeMap::new();
        for p in &pods {
            if let Some(i) = ordinal(sts.metadata.name(), p.metadata.name()) {
                by_ord.insert(i, p.clone());
            }
        }

        if !deleting {
            self.reconcile(&sts, &api, &by_ord, replicas, parallel, &update_rev).await?;
        }

        // Status.
        let current_rev = sts.status.as_ref().and_then(|s| s.current_revision.clone()).unwrap_or_else(|| update_rev.clone());
        let active: Vec<&Arc<Pod>> = by_ord.values().filter(|p| p.metadata.deletion_timestamp.is_none()).collect();
        let updated = active.iter().filter(|p| revision(p) == Some(&update_rev)).count() as i32;
        let mut st = StatefulSetStatus {
            observed_generation: sts.metadata.generation,
            replicas: active.len() as i32,
            ready_replicas: Some(active.iter().filter(|p| is_pod_ready(p)).count() as i32),
            available_replicas: Some(active.iter().filter(|p| is_pod_available(p, spec.min_ready_seconds.unwrap_or(0))).count() as i32),
            current_replicas: Some(active.iter().filter(|p| revision(p) == Some(&current_rev)).count() as i32),
            updated_replicas: Some(updated),
            current_revision: Some(current_rev.clone()),
            update_revision: Some(update_rev.clone()),
            collision_count: Some(0),
            conditions: sts.status.as_ref().and_then(|s| s.conditions.clone()),
        };
        if updated == replicas && st.ready_replicas == Some(replicas) {
            st.current_revision = Some(update_rev.clone());
            st.current_replicas = Some(updated);
        }
        if sts.status.as_ref() != Some(&st) {
            let mut n = (*sts).clone();
            n.status = Some(st);
            let sapi: Api<StatefulSet> = Api::namespaced(self.client.clone(), ns);
            match sapi.replace_status(&n).await {
                Ok(_) => {}
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(None)
    }

    async fn reconcile(
        &self,
        sts: &StatefulSet,
        api: &Api<Pod>,
        by_ord: &BTreeMap<i32, Arc<Pod>>,
        replicas: i32,
        parallel: bool,
        update_rev: &str,
    ) -> anyhow::Result<()> {
        // 1. Replace failed pods, create missing ones (in order unless Parallel).
        for i in 0..replicas {
            match by_ord.get(&i) {
                Some(p) if matches!(pod_phase(p), "Failed" | "Succeeded") && p.metadata.deletion_timestamp.is_none() => {
                    delete_ignore_missing(api, p.metadata.name()).await?;
                    if !parallel {
                        return Ok(());
                    }
                }
                Some(p) => {
                    if !parallel && !running_and_ready(p) {
                        return Ok(()); // wait for this ordinal before touching later ones
                    }
                }
                None => {
                    let pod = self.new_pod(sts, i, update_rev);
                    match api.create(&pod).await {
                        Ok(_) => self.recorder.event(
                            sts,
                            NORMAL,
                            "SuccessfulCreate",
                            format!("create Pod {} in StatefulSet {} successful", pod.metadata.name(), sts.metadata.name()),
                        ),
                        Err(e) if e.is_already_exists() => {}
                        Err(e) => return Err(e.into()),
                    }
                    if !parallel {
                        return Ok(());
                    }
                }
            }
        }
        // 2. Scale down: highest ordinal first.
        let condemned: Vec<&Arc<Pod>> = by_ord.range(replicas..).rev().map(|(_, p)| p).collect();
        for p in &condemned {
            if p.metadata.deletion_timestamp.is_some() {
                if !parallel {
                    return Ok(());
                }
                continue;
            }
            delete_ignore_missing(api, p.metadata.name()).await?;
            self.recorder.event(
                sts,
                NORMAL,
                "SuccessfulDelete",
                format!("delete Pod {} in StatefulSet {} successful", p.metadata.name(), sts.metadata.name()),
            );
            if !parallel {
                return Ok(());
            }
        }
        // 3. Rolling update from the highest ordinal down to the partition.
        let us: Value = sts.spec.as_ref().and_then(|s| s.update_strategy.clone()).unwrap_or_default();
        if us.get("type").and_then(Value::as_str) == Some("OnDelete") {
            return Ok(());
        }
        let partition = us.pointer("/rollingUpdate/partition").and_then(Value::as_i64).unwrap_or(0) as i32;
        let all_ready = (0..replicas).all(|i| by_ord.get(&i).is_some_and(|p| running_and_ready(p)));
        for i in (partition..replicas).rev() {
            if let Some(p) = by_ord.get(&i) {
                if revision(p).map(String::as_str) != Some(update_rev) {
                    if all_ready || parallel {
                        delete_ignore_missing(api, p.metadata.name()).await?;
                    }
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

async fn delete_ignore_missing(api: &Api<Pod>, name: &str) -> anyhow::Result<()> {
    match api.delete(name).await {
        Ok(_) => Ok(()),
        Err(e) if e.is_not_found() => Ok(()),
        Err(e) => Err(e.into()),
    }
}
