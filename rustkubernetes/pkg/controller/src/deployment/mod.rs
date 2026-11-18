//! Port of `pkg/controller/deployment`: Deployment → ReplicaSets.
//!
//! * The **new** ReplicaSet is the one whose pod template equals the
//!   Deployment's (ignoring the `pod-template-hash` label); all others are old.
//! * **RollingUpdate** (`rolling.go`): scale the new RS up within `maxSurge`,
//!   scale old RSs down while keeping `replicas - maxUnavailable` available
//!   (unhealthy old replicas are removed first).
//! * **Recreate** (`recreate.go`): scale old RSs to 0, wait for their pods to
//!   go away, then scale the new RS up.
//! * Revisions live in the `deployment.kubernetes.io/revision` annotation;
//!   old RSs beyond `revisionHistoryLimit` are deleted once a rollout completes.
//! * Status conditions `Available` and `Progressing` drive `kubectl rollout status`.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use k8s_api::apps::v1::{Deployment, DeploymentCondition, DeploymentStatus, ReplicaSet, ReplicaSetSpec};
use k8s_api::core::v1::{Pod, PodTemplateSpec};
use k8s_api::IntOrString;
use k8s_apimachinery::meta::{now_rfc3339, parse_time, ObjectMeta};
use k8s_client_go::informers::SharedInformerFactory;
use k8s_client_go::tools::cache::{split_key, Event, Store};
use k8s_client_go::tools::record::{EventRecorder, NORMAL, WARNING};
use k8s_client_go::util::workqueue::WorkQueue;
use k8s_client_go::{Api, Client, PatchType};
use serde_json::{json, Value};

use crate::controller_utils::*;
use crate::ControllerContext;

pub const REVISION_ANNOTATION: &str = "deployment.kubernetes.io/revision";
pub const DESIRED_REPLICAS_ANNOTATION: &str = "deployment.kubernetes.io/desired-replicas";
pub const MAX_REPLICAS_ANNOTATION: &str = "deployment.kubernetes.io/max-replicas";
pub const POD_TEMPLATE_HASH: &str = "pod-template-hash";

pub struct Controller {
    client: Client,
    d_store: Store<Deployment>,
    rs_store: Store<ReplicaSet>,
    pod_store: Store<Pod>,
    informers: Arc<SharedInformerFactory>,
    queue: Queue,
    recorder: EventRecorder,
    resync: Duration,
}

impl Controller {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let d_inf = ctx.informers.informer::<Deployment>();
        let rs_inf = ctx.informers.informer::<ReplicaSet>();
        let pod_inf = ctx.informers.informer::<Pod>();
        let c = Arc::new(Self {
            client: ctx.client.clone(),
            d_store: d_inf.store(),
            rs_store: rs_inf.store(),
            pod_store: pod_inf.store(),
            informers: ctx.informers.clone(),
            queue: WorkQueue::new(),
            recorder: ctx.recorder("deployment-controller"),
            resync: ctx.config.resync_period,
        });
        let me = c.clone();
        on_events(d_inf.subscribe(), move |ev| me.queue.add(key(ev.object().as_ref())));
        let me = c.clone();
        on_events(rs_inf.subscribe(), move |ev| {
            let rs = ev.object();
            match owner_key(&rs.metadata, "Deployment") {
                Some(k) => me.queue.add(k),
                None if rs.metadata.controller_ref().is_none() => {
                    let labels = rs.metadata.labels();
                    for d in me.d_store.list_namespace(rs.metadata.namespace()) {
                        if selector_of(d.spec.as_ref().and_then(|s| s.selector.as_ref())).matches(&labels) {
                            me.queue.add(key(d.as_ref()));
                        }
                    }
                }
                None => {}
            }
        });
        // Recreate waits for old pods to disappear.
        let me = c.clone();
        on_events(pod_inf.subscribe(), move |ev| {
            if let Event::Deleted(p) = ev {
                if let Some(rs_key) = owner_key(&p.metadata, "ReplicaSet") {
                    if let Some(rs) = me.rs_store.get(&rs_key) {
                        if let Some(dk) = owner_key(&rs.metadata, "Deployment") {
                            me.queue.add(dk);
                        }
                    }
                }
            }
        });
        c
    }

    pub async fn run(self: Arc<Self>, workers: usize) {
        self.informers.wait_for_cache_sync().await;
        let store = self.d_store.clone();
        resync_every(self.queue.clone(), self.resync, move || store.keys());
        let me = self.clone();
        run_workers("deployment-controller", self.queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.sync(&k).await }
        })
        .await;
    }

    fn rs_api(&self, ns: &str) -> Api<ReplicaSet> {
        Api::namespaced(self.client.clone(), ns)
    }

    async fn sync(&self, key: &str) -> SyncResult {
        let (ns, _) = split_key(key);
        let Some(d) = self.d_store.get(key) else { return Ok(None) };
        let d = (*d).clone();
        let spec = d.spec.clone().unwrap_or_default();
        let selector = selector_of(spec.selector.as_ref());
        if selector.is_empty() || selector.nothing {
            self.recorder.event(&d, WARNING, "SelectingAll", "This deployment is selecting all pods. A non-empty selector is required.");
            return Ok(None);
        }
        // Claim ReplicaSets.
        let owner = new_controller_ref(&d);
        let rs_api = self.rs_api(ns);
        let mut rs_list = vec![];
        for rs in self.rs_store.list_namespace(ns) {
            match rs.metadata.controller_ref() {
                Some(r) if r.uid == owner.uid => rs_list.push((*rs).clone()),
                None if selector.matches(&rs.metadata.labels()) && d.metadata.deletion_timestamp.is_none() => {
                    adopt(&rs_api, &rs.metadata, &owner).await?;
                    rs_list.push((*rs).clone());
                }
                _ => {}
            }
        }
        if d.metadata.deletion_timestamp.is_some() {
            return self.sync_status(&d, &rs_list, None, false).await;
        }
        if spec.paused.unwrap_or(false) || self.is_scaling_event(&d, &rs_list) {
            return self.sync_scale(&d, rs_list).await;
        }
        match spec.strategy.as_ref().and_then(|s| s.type_.as_deref()) {
            Some("Recreate") => self.rollout_recreate(&d, rs_list).await,
            _ => self.rollout_rolling(&d, rs_list).await,
        }
    }

    // -------------------------------------------------------------------
    // ReplicaSet bookkeeping
    // -------------------------------------------------------------------

    /// Returns (new RS if any, old RSs).
    fn split_new_old(&self, d: &Deployment, rs_list: &[ReplicaSet]) -> (Option<ReplicaSet>, Vec<ReplicaSet>) {
        let dt = template_value(&d.spec.as_ref().map(|s| s.template.clone()).unwrap_or_default());
        let mut sorted = rs_list.to_vec();
        sorted.sort_by(|a, b| a.metadata.creation_timestamp.cmp(&b.metadata.creation_timestamp));
        let new_idx = sorted.iter().position(|rs| template_value(&rs_template(rs)) == dt);
        let new = new_idx.map(|i| sorted.remove(i));
        (new, sorted)
    }

    fn is_scaling_event(&self, d: &Deployment, rs_list: &[ReplicaSet]) -> bool {
        let want = replicas_of(d).to_string();
        rs_list
            .iter()
            .filter(|rs| rs_replicas(rs) > 0)
            .any(|rs| annotation(&rs.metadata, DESIRED_REPLICAS_ANNOTATION).is_some_and(|a| *a != want))
    }

    /// `getNewReplicaSet`: finds or (optionally) creates the new RS and syncs revisions.
    async fn get_new_rs(&self, d: &Deployment, rs_list: &[ReplicaSet], create: bool) -> anyhow::Result<Option<ReplicaSet>> {
        let (existing, old) = self.split_new_old(d, rs_list);
        let max_old = old.iter().map(revision).max().unwrap_or(0);
        let new_revision = (max_old + 1).to_string();
        let ns = d.metadata.namespace();
        let spec = d.spec.clone().unwrap_or_default();
        if let Some(mut rs) = existing {
            let mut changed = false;
            let anns = rs.metadata.annotations.get_or_insert_with(Default::default);
            if anns.get(REVISION_ANNOTATION).and_then(|r| r.parse::<i64>().ok()).unwrap_or(0) < max_old + 1 {
                anns.insert(REVISION_ANNOTATION.into(), new_revision.clone());
                changed = true;
            }
            for (k, v) in replica_annotations(d) {
                if anns.get(&k) != Some(&v) {
                    anns.insert(k, v);
                    changed = true;
                }
            }
            let rs_spec = rs.spec.get_or_insert_with(Default::default);
            if rs_spec.min_ready_seconds != spec.min_ready_seconds {
                rs_spec.min_ready_seconds = spec.min_ready_seconds;
                changed = true;
            }
            if changed {
                rs = self.rs_api(ns).replace(&rs).await?;
            }
            self.sync_deployment_revision(d, revision(&rs)).await?;
            return Ok(Some(rs));
        }
        if !create {
            return Ok(None);
        }
        let collision = d.status.as_ref().and_then(|s| s.collision_count);
        let hash = compute_hash(&serde_json::to_value(&spec.template).unwrap_or_default(), collision);
        let mut template = spec.template.clone();
        template
            .metadata
            .get_or_insert_with(Default::default)
            .labels
            .get_or_insert_with(Default::default)
            .insert(POD_TEMPLATE_HASH.into(), hash.clone());
        let mut selector = spec.selector.clone().unwrap_or_default();
        selector.match_labels.get_or_insert_with(Default::default).insert(POD_TEMPLATE_HASH.into(), hash.clone());
        let mut anns = replica_annotations(d);
        anns.insert(REVISION_ANNOTATION.into(), new_revision.clone());
        let all: Vec<ReplicaSet> = rs_list.to_vec();
        let replicas = new_rs_new_replicas(d, &all, None);
        let mut rs = ReplicaSet::new(ObjectMeta {
            name: Some(format!("{}-{hash}", d.metadata.name())),
            namespace: Some(ns.into()),
            labels: template.metadata.as_ref().and_then(|m| m.labels.clone()),
            annotations: Some(anns),
            owner_references: Some(vec![new_controller_ref(d)]),
            ..Default::default()
        });
        rs.spec = Some(ReplicaSetSpec {
            replicas: Some(replicas),
            min_ready_seconds: spec.min_ready_seconds,
            selector: Some(selector),
            template: Some(template),
        });
        match self.rs_api(ns).create(&rs).await {
            Ok(created) => {
                self.recorder.event(
                    d,
                    NORMAL,
                    "ScalingReplicaSet",
                    format!("Scaled up replica set {} from 0 to {replicas}", created.metadata.name()),
                );
                self.sync_deployment_revision(d, revision(&created)).await?;
                Ok(Some(created))
            }
            Err(e) if e.is_already_exists() => {
                // Hash collision with a different template: bump collisionCount and retry.
                let found = self.rs_api(ns).get(rs.metadata.name()).await?;
                if template_value(&rs_template(&found)) == template_value(&spec.template)
                    && found.metadata.controller_ref().is_some_and(|r| r.uid == d.metadata.uid.clone().unwrap_or_default())
                {
                    return Ok(Some(found));
                }
                let mut nd = d.clone();
                let st = nd.status.get_or_insert_with(Default::default);
                st.collision_count = Some(st.collision_count.unwrap_or(0) + 1);
                Api::<Deployment>::namespaced(self.client.clone(), ns).replace_status(&nd).await?;
                anyhow::bail!("hash collision for replica set {}", rs.metadata.name())
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn sync_deployment_revision(&self, d: &Deployment, rev: i64) -> anyhow::Result<()> {
        if annotation(&d.metadata, REVISION_ANNOTATION) == Some(&rev.to_string()) {
            return Ok(());
        }
        let api: Api<Deployment> = Api::namespaced(self.client.clone(), d.metadata.namespace());
        api.patch(d.metadata.name(), PatchType::Merge, &json!({"metadata": {"annotations": {REVISION_ANNOTATION: rev.to_string()}}}))
            .await?;
        Ok(())
    }

    /// Sets an RS's replicas (+ desired/max annotations). Returns true if it changed.
    async fn scale_rs(&self, d: &Deployment, rs: &mut ReplicaSet, to: i32) -> anyhow::Result<bool> {
        let from = rs_replicas(rs);
        let anns = rs.metadata.annotations.get_or_insert_with(Default::default);
        let ann_changed = replica_annotations(d).into_iter().any(|(k, v)| anns.insert(k, v.clone()).as_ref() != Some(&v));
        if from == to && !ann_changed {
            return Ok(false);
        }
        rs.spec.get_or_insert_with(Default::default).replicas = Some(to);
        *rs = self.rs_api(d.metadata.namespace()).replace(rs).await?;
        if from != to {
            let dir = if to > from { "up" } else { "down" };
            self.recorder.event(
                d,
                NORMAL,
                "ScalingReplicaSet",
                format!("Scaled {dir} replica set {} from {from} to {to}", rs.metadata.name()),
            );
        }
        Ok(from != to)
    }

    // -------------------------------------------------------------------
    // Strategies
    // -------------------------------------------------------------------

    async fn rollout_rolling(&self, d: &Deployment, rs_list: Vec<ReplicaSet>) -> SyncResult {
        let created_or_found = self.get_new_rs(d, &rs_list, true).await?;
        let mut new_rs = created_or_found.expect("created");
        let (_, mut old) = self.split_new_old(d, &rs_list);
        old.retain(|rs| rs.metadata.uid != new_rs.metadata.uid);

        // Scale up the new RS within maxSurge.
        let all: Vec<ReplicaSet> = old.iter().cloned().chain(std::iter::once(new_rs.clone())).collect();
        let want = new_rs_new_replicas(d, &all, Some(&new_rs));
        if self.scale_rs(d, &mut new_rs, want).await? {
            return self.sync_status(d, &old, Some(&new_rs), true).await;
        }
        // Scale down old RSs within maxUnavailable.
        if self.reconcile_old(d, &mut old, &new_rs).await? {
            return self.sync_status(d, &old, Some(&new_rs), true).await;
        }
        if deployment_complete(d, &new_rs, &old) {
            self.cleanup_old(d, &old).await?;
        }
        self.sync_status(d, &old, Some(&new_rs), false).await
    }

    async fn reconcile_old(&self, d: &Deployment, old: &mut [ReplicaSet], new_rs: &ReplicaSet) -> anyhow::Result<bool> {
        let old_count: i32 = old.iter().map(rs_replicas).sum();
        if old_count == 0 {
            return Ok(false);
        }
        let replicas = replicas_of(d);
        let all_count = old_count + rs_replicas(new_rs);
        let (_, max_unavailable) = surge_and_unavailable(d);
        let min_available = replicas - max_unavailable;
        let new_unavailable = rs_replicas(new_rs) - rs_available(new_rs);
        let max_scaled_down = all_count - min_available - new_unavailable;
        if max_scaled_down <= 0 {
            return Ok(false);
        }
        old.sort_by(|a, b| a.metadata.creation_timestamp.cmp(&b.metadata.creation_timestamp));
        // 1. Remove unhealthy old replicas first (they do not count as available anyway).
        let mut cleaned = 0;
        let mut changed = false;
        for rs in old.iter_mut() {
            if cleaned >= max_scaled_down {
                break;
            }
            let unhealthy = rs_replicas(rs) - rs_available(rs);
            if rs_replicas(rs) == 0 || unhealthy <= 0 {
                continue;
            }
            let scaled = (max_scaled_down - cleaned).min(unhealthy);
            let to = rs_replicas(rs) - scaled;
            changed |= self.scale_rs(d, rs, to).await?;
            cleaned += scaled;
        }
        // 2. Scale down healthy old replicas while keeping minAvailable.
        let available: i32 = old.iter().map(rs_available).sum::<i32>() + rs_available(new_rs);
        let mut remaining = available - min_available;
        for rs in old.iter_mut() {
            if remaining <= 0 {
                break;
            }
            if rs_replicas(rs) == 0 {
                continue;
            }
            let down = rs_replicas(rs).min(remaining);
            let to = rs_replicas(rs) - down;
            changed |= self.scale_rs(d, rs, to).await?;
            remaining -= down;
        }
        Ok(changed)
    }

    async fn rollout_recreate(&self, d: &Deployment, rs_list: Vec<ReplicaSet>) -> SyncResult {
        let new_rs = self.get_new_rs(d, &rs_list, false).await?;
        let (_, mut old) = self.split_new_old(d, &rs_list);
        if let Some(n) = &new_rs {
            old.retain(|rs| rs.metadata.uid != n.metadata.uid);
        }
        let mut scaled_down = false;
        for rs in old.iter_mut() {
            scaled_down |= self.scale_rs(d, rs, 0).await?;
        }
        if scaled_down {
            return self.sync_status(d, &old, new_rs.as_ref(), true).await;
        }
        // Wait until every pod of the old ReplicaSets is gone.
        let old_uids: Vec<&str> = old.iter().filter_map(|rs| rs.metadata.uid.as_deref()).collect();
        let old_pods_running = self.pod_store.list_namespace(d.metadata.namespace()).iter().any(|p| {
            p.metadata.controller_ref().is_some_and(|r| old_uids.contains(&r.uid.as_str()))
                && !matches!(pod_phase(p), "Succeeded" | "Failed")
        });
        if old_pods_running {
            return self.sync_status(d, &old, new_rs.as_ref(), false).await;
        }
        let mut new_rs = match new_rs {
            Some(rs) => rs,
            None => self.get_new_rs(d, &rs_list, true).await?.expect("created"),
        };
        let changed = self.scale_rs(d, &mut new_rs, replicas_of(d)).await?;
        if !changed && deployment_complete(d, &new_rs, &old) {
            self.cleanup_old(d, &old).await?;
        }
        self.sync_status(d, &old, Some(&new_rs), changed).await
    }

    /// Paused deployments and replica-count changes: only scale, no rollout (`sync`).
    async fn sync_scale(&self, d: &Deployment, rs_list: Vec<ReplicaSet>) -> SyncResult {
        let new_rs = self.get_new_rs(d, &rs_list, false).await?;
        let (_, mut old) = self.split_new_old(d, &rs_list);
        if let Some(n) = &new_rs {
            old.retain(|rs| rs.metadata.uid != n.metadata.uid);
        }
        let mut new_rs = new_rs;
        let replicas = replicas_of(d);
        let mut active: Vec<&mut ReplicaSet> = old.iter_mut().filter(|rs| rs_replicas(rs) > 0).collect();
        match (&mut new_rs, active.len()) {
            // Only the new RS is active (or nothing is): it gets every replica.
            (Some(n), 0) => {
                self.scale_rs(d, n, replicas).await?;
            }
            (None, 1) => {
                let rs = active.pop().unwrap();
                self.scale_rs(d, rs, replicas).await?;
            }
            // A rollout is in progress: give the difference to the new RS (simplified
            // proportional scaling), keeping old RSs and refreshing their annotations.
            (Some(n), _) => {
                let (surge, _) = surge_and_unavailable(d);
                let current: i32 = active.iter().map(|rs| rs_replicas(rs)).sum::<i32>() + rs_replicas(n);
                let diff = replicas + surge - current;
                let to = (rs_replicas(n) + diff).clamp(0, replicas);
                self.scale_rs(d, n, to).await?;
                for rs in active {
                    let r = rs_replicas(rs);
                    self.scale_rs(d, rs, r).await?;
                }
            }
            (None, _) => {
                for rs in active {
                    let r = rs_replicas(rs);
                    self.scale_rs(d, rs, r).await?;
                }
            }
        }
        self.sync_status(d, &old, new_rs.as_ref(), false).await
    }

    /// Deletes old, fully scaled-down RSs beyond `revisionHistoryLimit`.
    async fn cleanup_old(&self, d: &Deployment, old: &[ReplicaSet]) -> anyhow::Result<()> {
        let limit = d.spec.as_ref().and_then(|s| s.revision_history_limit).unwrap_or(10).max(0) as usize;
        let mut candidates: Vec<&ReplicaSet> = old
            .iter()
            .filter(|rs| {
                rs_replicas(rs) == 0 && rs.status.as_ref().map(|s| s.replicas).unwrap_or(0) == 0 && rs.metadata.deletion_timestamp.is_none()
            })
            .collect();
        if candidates.len() <= limit {
            return Ok(());
        }
        candidates.sort_by_key(|rs| revision(rs));
        let excess = candidates.len() - limit;
        for rs in candidates.into_iter().take(excess) {
            match self.rs_api(d.metadata.namespace()).delete(rs.metadata.name()).await {
                Ok(_) => {}
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------------
    // Status
    // -------------------------------------------------------------------

    async fn sync_status(&self, d: &Deployment, old: &[ReplicaSet], new_rs: Option<&ReplicaSet>, progressed: bool) -> SyncResult {
        let all: Vec<&ReplicaSet> = old.iter().chain(new_rs).collect();
        let replicas = replicas_of(d);
        let prev = d.status.clone().unwrap_or_default();
        let mut st = DeploymentStatus {
            observed_generation: d.metadata.generation,
            replicas: Some(all.iter().map(|rs| rs.status.as_ref().map(|s| s.replicas).unwrap_or(0)).sum()),
            updated_replicas: Some(new_rs.and_then(|rs| rs.status.as_ref()).map(|s| s.replicas).unwrap_or(0)),
            ready_replicas: Some(all.iter().map(|rs| rs.status.as_ref().and_then(|s| s.ready_replicas).unwrap_or(0)).sum()),
            available_replicas: Some(all.iter().map(|rs| rs_available(rs)).sum()),
            unavailable_replicas: None,
            conditions: prev.conditions.clone(),
            collision_count: prev.collision_count,
        };
        let total_spec: i32 = all.iter().map(|rs| rs_replicas(rs)).sum();
        st.unavailable_replicas = Some((total_spec - st.available_replicas.unwrap()).max(0));

        let now = now_rfc3339();
        let (_, max_unavailable) = surge_and_unavailable(d);
        if st.available_replicas.unwrap() >= replicas - max_unavailable {
            set_condition(&mut st, "Available", "True", "MinimumReplicasAvailable", "Deployment has minimum availability.", &now, false);
        } else {
            set_condition(
                &mut st,
                "Available",
                "False",
                "MinimumReplicasUnavailable",
                "Deployment does not have minimum availability.",
                &now,
                false,
            );
        }

        let paused = d.spec.as_ref().and_then(|s| s.paused).unwrap_or(false);
        let rs_name = new_rs.map(|rs| rs.metadata.name().to_string()).unwrap_or_default();
        let deadline = d.spec.as_ref().and_then(|s| s.progress_deadline_seconds);
        let complete = st.updated_replicas == Some(replicas)
            && st.replicas == Some(replicas)
            && st.available_replicas == Some(replicas)
            && new_rs.is_some();
        let mut requeue = None;
        if paused {
            set_condition(&mut st, "Progressing", "Unknown", "DeploymentPaused", "Deployment is paused", &now, true);
        } else if complete {
            set_condition(
                &mut st,
                "Progressing",
                "True",
                "NewReplicaSetAvailable",
                &format!("ReplicaSet \"{rs_name}\" has successfully progressed."),
                &now,
                true,
            );
        } else {
            let made_progress = progressed
                || st.updated_replicas > prev.updated_replicas
                || st.ready_replicas > prev.ready_replicas
                || st.available_replicas > prev.available_replicas
                || prev.observed_generation != d.metadata.generation;
            let cond = st.conditions.iter().flatten().find(|c| c.type_ == "Progressing").cloned();
            let timed_out = !made_progress
                && cond.as_ref().is_some_and(|c| c.reason.as_deref() != Some("ProgressDeadlineExceeded"))
                && deadline
                    .zip(cond.as_ref().and_then(|c| c.last_update_time.as_deref()).and_then(parse_time))
                    .is_some_and(|(dl, t)| t + chrono::Duration::seconds(dl as i64) < Utc::now());
            if timed_out {
                set_condition(
                    &mut st,
                    "Progressing",
                    "False",
                    "ProgressDeadlineExceeded",
                    &format!("ReplicaSet \"{rs_name}\" has timed out progressing."),
                    &now,
                    true,
                );
            } else if made_progress || cond.is_none() || cond.as_ref().is_some_and(|c| c.reason.as_deref() == Some("DeploymentPaused")) {
                let reason = if cond.is_none() { "NewReplicaSetCreated" } else { "ReplicaSetUpdated" };
                let msg = if cond.is_none() {
                    format!("Created new replica set \"{rs_name}\"")
                } else {
                    format!("ReplicaSet \"{rs_name}\" is progressing.")
                };
                set_condition(&mut st, "Progressing", "True", reason, &msg, &now, true);
            }
            if let Some(dl) = deadline {
                requeue = Some(Duration::from_secs((dl as u64).min(10)));
            }
        }

        if d.status.as_ref() != Some(&st) {
            let mut nd = d.clone();
            nd.status = Some(st);
            let api: Api<Deployment> = Api::namespaced(self.client.clone(), d.metadata.namespace());
            match api.replace_status(&nd).await {
                Ok(_) => {}
                Err(e) if e.is_not_found() => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(requeue)
    }
}

// ---------------------------------------------------------------------------
// helpers (deploymentutil)
// ---------------------------------------------------------------------------

fn annotation<'a>(m: &'a ObjectMeta, k: &str) -> Option<&'a String> {
    m.annotations.as_ref()?.get(k)
}

fn revision(rs: &ReplicaSet) -> i64 {
    annotation(&rs.metadata, REVISION_ANNOTATION).and_then(|r| r.parse().ok()).unwrap_or(0)
}

fn replicas_of(d: &Deployment) -> i32 {
    d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1)
}

fn rs_replicas(rs: &ReplicaSet) -> i32 {
    rs.spec.as_ref().and_then(|s| s.replicas).unwrap_or(0)
}

fn rs_available(rs: &ReplicaSet) -> i32 {
    rs.status.as_ref().and_then(|s| s.available_replicas).unwrap_or(0)
}

fn rs_template(rs: &ReplicaSet) -> PodTemplateSpec {
    rs.spec.as_ref().and_then(|s| s.template.clone()).unwrap_or_default()
}

/// Template as JSON without the `pod-template-hash` label (`EqualIgnoreHash`).
fn template_value(t: &PodTemplateSpec) -> Value {
    let mut v = serde_json::to_value(t).unwrap_or_default();
    if let Some(labels) = v.pointer_mut("/metadata/labels").and_then(Value::as_object_mut) {
        labels.remove(POD_TEMPLATE_HASH);
        if labels.is_empty() {
            v["metadata"].as_object_mut().unwrap().remove("labels");
        }
    }
    normalize_template(&v)
}

/// `(maxSurge, maxUnavailable)` resolved against `replicas` (`ResolveFenceposts`).
fn surge_and_unavailable(d: &Deployment) -> (i32, i32) {
    let replicas = replicas_of(d) as i64;
    let ru = d.spec.as_ref().and_then(|s| s.strategy.as_ref()).and_then(|s| s.rolling_update.clone());
    if d.spec.as_ref().and_then(|s| s.strategy.as_ref()).and_then(|s| s.type_.as_deref()) == Some("Recreate") {
        return (0, replicas as i32);
    }
    let default = IntOrString::String("25%".into());
    let surge = ru.as_ref().and_then(|r| r.max_surge.clone()).unwrap_or(default.clone()).scaled_value(replicas, true).unwrap_or(1);
    let mut unavailable = ru.as_ref().and_then(|r| r.max_unavailable.clone()).unwrap_or(default).scaled_value(replicas, false).unwrap_or(0);
    if surge == 0 && unavailable == 0 {
        unavailable = 1;
    }
    (surge as i32, unavailable as i32)
}

fn replica_annotations(d: &Deployment) -> std::collections::BTreeMap<String, String> {
    let (surge, _) = surge_and_unavailable(d);
    let r = replicas_of(d);
    [(DESIRED_REPLICAS_ANNOTATION.to_string(), r.to_string()), (MAX_REPLICAS_ANNOTATION.to_string(), (r + surge).to_string())]
        .into_iter()
        .collect()
}

/// `NewRSNewReplicas`: how many replicas the new RS should have now.
fn new_rs_new_replicas(d: &Deployment, all: &[ReplicaSet], new_rs: Option<&ReplicaSet>) -> i32 {
    let replicas = replicas_of(d);
    let is_recreate = d.spec.as_ref().and_then(|s| s.strategy.as_ref()).and_then(|s| s.type_.as_deref()) == Some("Recreate");
    if is_recreate {
        return replicas;
    }
    let new_current = new_rs.map(rs_replicas).unwrap_or(0);
    if new_current >= replicas {
        return replicas;
    }
    let (surge, _) = surge_and_unavailable(d);
    let current: i32 = all.iter().map(rs_replicas).sum();
    let max_total = replicas + surge;
    if current >= max_total {
        return new_current;
    }
    new_current + (max_total - current).min(replicas - new_current)
}

fn deployment_complete(d: &Deployment, new_rs: &ReplicaSet, old: &[ReplicaSet]) -> bool {
    let r = replicas_of(d);
    rs_replicas(new_rs) == r && rs_available(new_rs) == r && old.iter().all(|rs| rs_replicas(rs) == 0)
}

fn set_condition(st: &mut DeploymentStatus, type_: &str, status: &str, reason: &str, message: &str, now: &str, update_time: bool) {
    let conds = st.conditions.get_or_insert_with(Vec::new);
    match conds.iter_mut().find(|c| c.type_ == type_) {
        Some(c) => {
            let same = c.status == status && c.reason.as_deref() == Some(reason);
            if c.status != status {
                c.last_transition_time = Some(now.into());
            }
            if !same || update_time && c.message.as_deref() != Some(message) || reason == "ReplicaSetUpdated" {
                c.last_update_time = Some(now.into());
            }
            c.status = status.into();
            c.reason = Some(reason.into());
            c.message = Some(message.into());
        }
        None => conds.push(DeploymentCondition {
            type_: type_.into(),
            status: status.into(),
            last_update_time: Some(now.into()),
            last_transition_time: Some(now.into()),
            reason: Some(reason.into()),
            message: Some(message.into()),
        }),
    }
    conds.sort_by(|a, b| a.type_.cmp(&b.type_));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deploy(replicas: i32, surge: &str, unavail: &str) -> Deployment {
        serde_json::from_value(json!({"spec": {"replicas": replicas,
            "strategy": {"type": "RollingUpdate", "rollingUpdate": {"maxSurge": surge, "maxUnavailable": unavail}}}}))
        .unwrap()
    }

    fn rs(replicas: i32) -> ReplicaSet {
        serde_json::from_value(json!({"spec": {"replicas": replicas}})).unwrap()
    }

    #[test]
    fn fenceposts() {
        assert_eq!(surge_and_unavailable(&deploy(10, "25%", "25%")), (3, 2));
        assert_eq!(surge_and_unavailable(&deploy(1, "0%", "0%")), (0, 1));
    }

    #[test]
    fn new_replicas_respect_surge() {
        let d = deploy(10, "25%", "25%");
        // fresh rollout: 10 old + 0 new, surge 3 → new gets 3
        assert_eq!(new_rs_new_replicas(&d, &[rs(10), rs(0)], Some(&rs(0))), 3);
        // already at max total
        assert_eq!(new_rs_new_replicas(&d, &[rs(10), rs(3)], Some(&rs(3))), 3);
        // first deployment: nothing exists → all replicas (capped by surge budget)
        assert_eq!(new_rs_new_replicas(&d, &[], None), 10);
    }

    #[test]
    fn template_equality_ignores_hash() {
        let a: PodTemplateSpec = serde_json::from_value(json!({"metadata": {"labels": {"app": "x", "pod-template-hash": "abc"}}})).unwrap();
        let b: PodTemplateSpec = serde_json::from_value(json!({"metadata": {"labels": {"app": "x"}}})).unwrap();
        assert_eq!(template_value(&a), template_value(&b));
    }
}
