//! Port of `pkg/controller/replicaset`: keeps `spec.replicas` pods running.
//!
//! `sync(key)`:
//! 1. claim pods (adopt matching orphans, release non-matching owned pods);
//! 2. if expectations are satisfied, create (slow-start batches) or delete
//!    (ranked: unscheduled/pending/not-ready/newest first) pods;
//! 3. write `status` (replicas, fullyLabeled, ready, available, observedGeneration).

use std::sync::Arc;
use std::time::Duration;

use k8s_api::apps::v1::{ReplicaSet, ReplicaSetStatus};
use k8s_api::core::v1::Pod;
use k8s_apimachinery::labels::Selector;
use k8s_client_go::tools::cache::{split_key, Event, Store};
use k8s_client_go::tools::record::{EventRecorder, NORMAL, WARNING};
use k8s_client_go::util::workqueue::WorkQueue;
use k8s_client_go::{Api, Client};

use crate::controller_utils::*;
use crate::ControllerContext;

/// Max pods created or deleted in one sync (`burstReplicas`).
const BURST_REPLICAS: usize = 500;

pub struct Controller {
    client: Client,
    rs_store: Store<ReplicaSet>,
    pod_store: Store<Pod>,
    rs_inf: Arc<k8s_client_go::tools::cache::Informer<ReplicaSet>>,
    pod_inf: Arc<k8s_client_go::tools::cache::Informer<Pod>>,
    queue: Queue,
    expectations: Expectations,
    recorder: EventRecorder,
    resync: Duration,
}

impl Controller {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let rs_inf = ctx.informers.informer::<ReplicaSet>();
        let pod_inf = ctx.informers.informer::<Pod>();
        let c = Arc::new(Self {
            client: ctx.client.clone(),
            rs_store: rs_inf.store(),
            pod_store: pod_inf.store(),
            rs_inf: rs_inf.clone(),
            pod_inf: pod_inf.clone(),
            queue: WorkQueue::new(),
            expectations: Expectations::default(),
            recorder: ctx.recorder("replicaset-controller"),
            resync: ctx.config.resync_period,
        });
        let me = c.clone();
        on_events(rs_inf.subscribe(), move |ev| me.queue.add(key(ev.object().as_ref())));
        let me = c.clone();
        on_events(pod_inf.subscribe(), move |ev| me.handle_pod(ev));
        c
    }

    /// ReplicaSets in the pod's namespace whose selector matches it (for orphans).
    fn matching_sets(&self, pod: &Pod) -> Vec<String> {
        let labels = pod.metadata.labels();
        self.rs_store
            .list_namespace(pod.metadata.namespace())
            .into_iter()
            .filter(|rs| selector_of(rs.spec.as_ref().and_then(|s| s.selector.as_ref())).matches(&labels))
            .map(|rs| key(rs.as_ref()))
            .collect()
    }

    fn enqueue_owner_or_matching(&self, pod: &Pod) {
        match owner_key(&pod.metadata, "ReplicaSet") {
            Some(k) => self.queue.add(k),
            None if pod.metadata.controller_ref().is_none() => self.matching_sets(pod).into_iter().for_each(|k| self.queue.add(k)),
            None => {}
        }
    }

    fn handle_pod(&self, ev: Event<Pod>) {
        match ev {
            Event::Added(p) => {
                if let Some(k) = owner_key(&p.metadata, "ReplicaSet") {
                    if p.metadata.deletion_timestamp.is_some() {
                        self.expectations.deletion_observed(&k);
                    } else {
                        self.expectations.creation_observed(&k);
                    }
                }
                self.enqueue_owner_or_matching(&p);
            }
            Event::Updated { old, new } => {
                if old.metadata.controller_ref() != new.metadata.controller_ref() {
                    self.enqueue_owner_or_matching(&old);
                }
                if new.metadata.deletion_timestamp.is_some() && old.metadata.deletion_timestamp.is_none() {
                    if let Some(k) = owner_key(&new.metadata, "ReplicaSet") {
                        self.expectations.deletion_observed(&k);
                    }
                }
                self.enqueue_owner_or_matching(&new);
            }
            Event::Deleted(p) => {
                if let Some(k) = owner_key(&p.metadata, "ReplicaSet") {
                    if p.metadata.deletion_timestamp.is_none() {
                        self.expectations.deletion_observed(&k);
                    }
                    self.queue.add(k);
                }
            }
        }
    }

    pub async fn run(self: Arc<Self>, workers: usize) {
        self.rs_inf.wait_synced().await;
        self.pod_inf.wait_synced().await;
        let store = self.rs_store.clone();
        resync_every(self.queue.clone(), self.resync, move || store.keys());
        let me = self.clone();
        run_workers("replicaset-controller", self.queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.sync(&k).await }
        })
        .await;
    }

    async fn sync(&self, key: &str) -> SyncResult {
        let (ns, _) = split_key(key);
        let Some(rs) = self.rs_store.get(key) else {
            self.expectations.delete(key);
            return Ok(None);
        };
        let spec = rs.spec.clone().unwrap_or_default();
        let selector = selector_of(spec.selector.as_ref());
        let pods_api: Api<Pod> = Api::namespaced(self.client.clone(), ns);
        let active: Vec<Arc<Pod>> = self.pod_store.list_namespace(ns).into_iter().filter(|p| is_pod_active(p)).collect();
        let owner = new_controller_ref(rs.as_ref());
        let deleting = rs.metadata.deletion_timestamp.is_some();
        let pods = claim_pods(&pods_api, active, &owner, &selector, deleting).await?;

        let mut sync_err = None;
        if self.expectations.satisfied(key) && !deleting {
            if let Err(e) = self.manage_replicas(key, &rs, &pods, &pods_api).await {
                sync_err = Some(e);
            }
        }

        let min_ready = spec.min_ready_seconds.unwrap_or(0);
        let status = calculate_status(&rs, &pods, min_ready);
        if rs.status.as_ref() != Some(&status) {
            let mut updated = (*rs).clone();
            updated.status = Some(status.clone());
            let api: Api<ReplicaSet> = Api::namespaced(self.client.clone(), ns);
            match api.replace_status(&updated).await {
                Ok(_) => {}
                Err(e) if e.is_not_found() => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        }
        if let Some(e) = sync_err {
            return Err(e);
        }
        let ready = status.ready_replicas.unwrap_or(0);
        if min_ready > 0 && ready != status.available_replicas.unwrap_or(0) {
            return Ok(Some(Duration::from_secs(min_ready as u64)));
        }
        Ok(None)
    }

    async fn manage_replicas(&self, key: &str, rs: &ReplicaSet, pods: &[Arc<Pod>], api: &Api<Pod>) -> anyhow::Result<()> {
        let spec = rs.spec.clone().unwrap_or_default();
        let want = spec.replicas.unwrap_or(1).max(0) as usize;
        let have = pods.len();
        if have < want {
            let n = (want - have).min(BURST_REPLICAS);
            self.expectations.expect(key, n as i64, 0);
            let template = spec.template.clone().unwrap_or_default();
            let owner = new_controller_ref(rs);
            let ns = rs.metadata.namespace();
            let mut created = 0;
            for batch in slow_start_batches(n) {
                let futs = (0..batch).map(|_| {
                    let pod = pod_from_template(&template, ns, &format!("{}-", rs.metadata.name()), Some(owner.clone()));
                    let api = api.clone();
                    async move { api.create(&pod).await }
                });
                let results = futures::future::join_all(futs).await;
                let mut failed = 0;
                for r in results {
                    match r {
                        Ok(p) => {
                            created += 1;
                            self.recorder.event(rs, NORMAL, "SuccessfulCreate", format!("Created pod: {}", p.metadata.name()));
                        }
                        Err(e) => {
                            failed += 1;
                            self.recorder.event(rs, WARNING, "FailedCreate", format!("Error creating: {e}"));
                        }
                    }
                }
                if failed > 0 {
                    // Pods we will not create are not expected either.
                    self.expectations.lower(key, (n - created) as i64, 0);
                    anyhow::bail!("failed to create {failed} pods");
                }
            }
        } else if have > want {
            let n = (have - want).min(BURST_REPLICAS);
            let mut victims: Vec<Arc<Pod>> = pods.to_vec();
            rank_for_deletion(&mut victims);
            victims.truncate(n);
            self.expectations.expect(key, 0, n as i64);
            let futs = victims.iter().map(|p| {
                let api = api.clone();
                let name = p.metadata.name().to_string();
                async move { (name.clone(), api.delete(&name).await) }
            });
            for (name, r) in futures::future::join_all(futs).await {
                match r {
                    Ok(_) => self.recorder.event(rs, NORMAL, "SuccessfulDelete", format!("Deleted pod: {name}")),
                    Err(e) if e.is_not_found() => self.expectations.deletion_observed(key),
                    Err(e) => {
                        self.expectations.deletion_observed(key);
                        return Err(e.into());
                    }
                }
            }
        }
        Ok(())
    }
}

fn calculate_status(rs: &ReplicaSet, pods: &[Arc<Pod>], min_ready: i32) -> ReplicaSetStatus {
    let tlabels =
        rs.spec.as_ref().and_then(|s| s.template.as_ref()).and_then(|t| t.metadata.as_ref()).map(|m| m.labels()).unwrap_or_default();
    let template_sel = Selector::from_map(&tlabels);
    let mut st = rs.status.clone().unwrap_or_default();
    st.replicas = pods.len() as i32;
    st.fully_labeled_replicas = Some(pods.iter().filter(|p| template_sel.matches(&p.metadata.labels())).count() as i32);
    st.ready_replicas = Some(pods.iter().filter(|p| is_pod_ready(p)).count() as i32);
    st.available_replicas = Some(pods.iter().filter(|p| is_pod_available(p, min_ready)).count() as i32);
    st.observed_generation = rs.metadata.generation;
    st
}
