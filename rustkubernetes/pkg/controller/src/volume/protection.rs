//! Port of `pkg/controller/volume/{pvcprotection,pvprotection}`.
//!
//! * `persistentvolumeclaim-protection-controller`: a deleted claim keeps its
//!   `kubernetes.io/pvc-protection` finalizer while a pod that is not finished
//!   uses it (`persistentVolumeClaim` or a generic ephemeral volume).
//! * `persistentvolume-protection-controller`: a deleted volume keeps
//!   `kubernetes.io/pv-protection` while it is `Bound`.

use std::sync::Arc;

use k8s_api::core::v1::{PersistentVolume, PersistentVolumeClaim, Pod};
use k8s_client_go::informers::SharedInformerFactory;
use k8s_client_go::tools::cache::{split_key, Event, Store};
use k8s_client_go::util::workqueue::WorkQueue;
use k8s_client_go::{Api, Client};
use k8s_component_helpers::storage::volume::{pod_claim_names, volume_phase, PVC_PROTECTION_FINALIZER, PV_PROTECTION_FINALIZER};

use crate::controller_utils::*;
use crate::ControllerContext;

fn has_finalizer(m: &k8s_apimachinery::meta::ObjectMeta, f: &str) -> bool {
    m.finalizers.as_ref().is_some_and(|fs| fs.iter().any(|x| x == f))
}

fn remove_finalizer(m: &mut k8s_apimachinery::meta::ObjectMeta, f: &str) {
    if let Some(fs) = m.finalizers.as_mut() {
        fs.retain(|x| x != f);
    }
}

pub struct PvcProtection {
    client: Client,
    claims: Store<PersistentVolumeClaim>,
    pods: Store<Pod>,
    informers: Arc<SharedInformerFactory>,
    queue: Queue,
}

impl PvcProtection {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let pvc_inf = ctx.informers.informer::<PersistentVolumeClaim>();
        let pod_inf = ctx.informers.informer::<Pod>();
        let c = Arc::new(Self {
            client: ctx.client.clone(),
            claims: pvc_inf.store(),
            pods: pod_inf.store(),
            informers: ctx.informers.clone(),
            queue: WorkQueue::new(),
        });
        let me = c.clone();
        on_events(pvc_inf.subscribe(), move |ev| me.queue.add(key(ev.object().as_ref())));
        // A pod going away (or finishing) may free its claims.
        let me = c.clone();
        on_events(pod_inf.subscribe(), move |ev| {
            let p = ev.object();
            if matches!(ev, Event::Deleted(_)) || matches!(pod_phase(p), "Succeeded" | "Failed") {
                for n in pod_claim_names(p) {
                    me.queue.add(format!("{}/{n}", p.metadata.namespace()));
                }
            }
        });
        c
    }

    pub async fn run(self: Arc<Self>, workers: usize) {
        self.informers.wait_for_cache_sync().await;
        let me = self.clone();
        run_workers("persistentvolumeclaim-protection-controller", self.queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.sync(&k).await }
        })
        .await;
    }

    /// `isBeingUsed`: a pod that is not terminated references the claim.
    fn in_use(&self, ns: &str, name: &str) -> bool {
        self.pods
            .list_namespace(ns)
            .iter()
            .filter(|p| !matches!(pod_phase(p), "Succeeded" | "Failed"))
            .any(|p| pod_claim_names(p).iter().any(|n| n == name))
    }

    async fn sync(&self, k: &str) -> SyncResult {
        let (ns, name) = split_key(k);
        let Some(c) = self.claims.get_ns(ns, name) else { return Ok(None) };
        if c.metadata.deletion_timestamp.is_none() || !has_finalizer(&c.metadata, PVC_PROTECTION_FINALIZER) {
            return Ok(None);
        }
        if self.in_use(ns, name) {
            return Ok(None);
        }
        let api: Api<PersistentVolumeClaim> = Api::namespaced(self.client.clone(), ns);
        // Double-check with a live read (`askAPIServer`) before letting the claim go.
        let live_pods: Api<Pod> = Api::namespaced(self.client.clone(), ns);
        let pods = live_pods.list(&Default::default()).await?;
        if pods
            .items
            .iter()
            .filter(|p| !matches!(pod_phase(p), "Succeeded" | "Failed"))
            .any(|p| pod_claim_names(p).iter().any(|n| n == name))
        {
            return Ok(None);
        }
        let mut c2 = (*c).clone();
        remove_finalizer(&mut c2.metadata, PVC_PROTECTION_FINALIZER);
        match api.replace(&c2).await {
            Ok(_) => tracing::debug!(pvc = k, "removed protection finalizer from PVC"),
            Err(e) if e.is_not_found() => {}
            Err(e) => return Err(e.into()),
        }
        Ok(None)
    }
}

pub struct PvProtection {
    client: Client,
    volumes: Store<PersistentVolume>,
    informers: Arc<SharedInformerFactory>,
    queue: Queue,
}

impl PvProtection {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let pv_inf = ctx.informers.informer::<PersistentVolume>();
        let c = Arc::new(Self {
            client: ctx.client.clone(),
            volumes: pv_inf.store(),
            informers: ctx.informers.clone(),
            queue: WorkQueue::new(),
        });
        let me = c.clone();
        on_events(pv_inf.subscribe(), move |ev| me.queue.add(key(ev.object().as_ref())));
        c
    }

    pub async fn run(self: Arc<Self>, workers: usize) {
        self.informers.wait_for_cache_sync().await;
        let me = self.clone();
        run_workers("persistentvolume-protection-controller", self.queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.sync(&k).await }
        })
        .await;
    }

    async fn sync(&self, k: &str) -> SyncResult {
        let Some(v) = self.volumes.get(k) else { return Ok(None) };
        if v.metadata.deletion_timestamp.is_none() || !has_finalizer(&v.metadata, PV_PROTECTION_FINALIZER) {
            return Ok(None);
        }
        // `isBeingUsed`: a Bound volume stays.
        if volume_phase(&v) == "Bound" {
            return Ok(None);
        }
        let mut v2 = (*v).clone();
        remove_finalizer(&mut v2.metadata, PV_PROTECTION_FINALIZER);
        let api: Api<PersistentVolume> = Api::all(self.client.clone());
        match api.replace(&v2).await {
            Ok(_) => Ok(None),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
