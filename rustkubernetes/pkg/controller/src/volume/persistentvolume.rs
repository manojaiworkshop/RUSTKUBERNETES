//! Port of `pkg/controller/volume/persistentvolume` (`persistentvolume-binder-controller`).
//!
//! Claims (`syncClaim`):
//! * unbound, no `volumeName`: bind the smallest matching Available volume
//!   (class, access modes, size, selector, volume mode); `WaitForFirstConsumer`
//!   claims wait for the scheduler's `volume.kubernetes.io/selected-node`;
//!   otherwise give a class-less claim the default class, provision through the
//!   class's provisioner (in-tree `kubernetes.io/host-path` with
//!   `--enable-hostpath-provisioner`, else an external provisioner is asked via
//!   `volume.kubernetes.io/storage-provisioner`), or stay Pending;
//! * pre-bound (`volumeName` set): bind to that volume if it fits;
//! * bound: `Lost` if the volume disappeared.
//!
//! Volumes (`syncVolume`): `Available` without a claim, `Bound` while the claim
//! exists, `Released` when it is gone, then reclaimed per policy — `Retain`
//! keeps it, `Delete` deletes it (and, for host-path volumes under `/tmp`,
//! their directory), `Recycle` is not supported.

use std::sync::Arc;
use std::time::Duration;

use k8s_api::core::v1::{PersistentVolume, PersistentVolumeClaim, Pod};
use k8s_api::storage::v1::StorageClass;
use k8s_client_go::informers::SharedInformerFactory;
use k8s_client_go::tools::cache::{split_key, Event, Store};
use k8s_client_go::tools::record::{EventRecorder, NORMAL, WARNING};
use k8s_client_go::util::workqueue::WorkQueue;
use k8s_client_go::{Api, Client};
use k8s_component_helpers::storage::volume::*;
use serde_json::{json, Value};

use crate::controller_utils::*;
use crate::ControllerContext;

/// The in-tree host-path volume plugin (`kubernetes.io/host-path`).
pub const HOST_PATH_PLUGIN: &str = "kubernetes.io/host-path";
/// Where the host-path provisioner creates volumes (`/tmp/hostpath_pv/<uuid>`).
pub const HOST_PATH_BASE: &str = "/tmp/hostpath_pv";

pub struct Controller {
    client: Client,
    volumes: Store<PersistentVolume>,
    claims: Store<PersistentVolumeClaim>,
    classes: Store<StorageClass>,
    pods: Store<Pod>,
    informers: Arc<SharedInformerFactory>,
    claim_queue: Queue,
    volume_queue: Queue,
    recorder: EventRecorder,
    hostpath_provisioner: bool,
    sync_period: Duration,
}

fn set_path(v: &mut Option<Value>, key: &str, val: Value) {
    let o = v.get_or_insert_with(|| json!({}));
    o[key] = val;
}

impl Controller {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let pv_inf = ctx.informers.informer::<PersistentVolume>();
        let pvc_inf = ctx.informers.informer::<PersistentVolumeClaim>();
        let sc_inf = ctx.informers.informer::<StorageClass>();
        let pod_inf = ctx.informers.informer::<Pod>();
        let c = Arc::new(Self {
            client: ctx.client.clone(),
            volumes: pv_inf.store(),
            claims: pvc_inf.store(),
            classes: sc_inf.store(),
            pods: pod_inf.store(),
            informers: ctx.informers.clone(),
            claim_queue: WorkQueue::new(),
            volume_queue: WorkQueue::new(),
            recorder: ctx.recorder("persistentvolume-controller"),
            hostpath_provisioner: ctx.config.volume.enable_hostpath_provisioner,
            sync_period: ctx.config.volume.pv_claim_binder_sync_period,
        });
        let me = c.clone();
        on_events(pv_inf.subscribe(), move |ev| {
            let v = ev.object();
            me.volume_queue.add(key(v.as_ref()));
            // A volume change may finish a claim's binding.
            if let Some(r) = claim_ref(v.as_ref()) {
                me.claim_queue.add(format!("{}/{}", r["namespace"].as_str().unwrap_or(""), r["name"].as_str().unwrap_or("")));
            }
        });
        let me = c.clone();
        on_events(pvc_inf.subscribe(), move |ev| {
            let claim = ev.object();
            me.claim_queue.add(key(claim.as_ref()));
            // A deleted claim releases its volume.
            if matches!(ev, Event::Deleted(_)) {
                let vn = claim_volume_name(claim.as_ref());
                if !vn.is_empty() {
                    me.volume_queue.add(vn.to_string());
                }
            }
        });
        c
    }

    pub async fn run(self: Arc<Self>, workers: usize) {
        self.informers.wait_for_cache_sync().await;
        let (vs, cs) = (self.volumes.clone(), self.claims.clone());
        resync_every(self.volume_queue.clone(), self.sync_period, move || vs.keys());
        resync_every(self.claim_queue.clone(), self.sync_period, move || cs.keys());
        let me = self.clone();
        let volumes = run_workers("persistentvolume-binder-controller", self.volume_queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.sync_volume_key(&k).await }
        });
        let me = self.clone();
        let claims = run_workers("persistentvolume-binder-controller", self.claim_queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.sync_claim_key(&k).await }
        });
        futures::join!(volumes, claims);
    }

    fn pv_api(&self) -> Api<PersistentVolume> {
        Api::all(self.client.clone())
    }

    fn pvc_api(&self, ns: &str) -> Api<PersistentVolumeClaim> {
        Api::namespaced(self.client.clone(), ns)
    }

    fn class(&self, name: &str) -> Option<Arc<StorageClass>> {
        (!name.is_empty()).then(|| self.classes.get(name)).flatten()
    }

    /// `IsDelayBindingMode`
    fn delay_binding(&self, c: &PersistentVolumeClaim) -> bool {
        self.class(&claim_class(c)).is_some_and(|sc| sc.wait_for_first_consumer())
    }

    // -----------------------------------------------------------------------
    // claims
    // -----------------------------------------------------------------------

    async fn sync_claim_key(&self, k: &str) -> SyncResult {
        let (ns, name) = split_key(k);
        let Some(c) = self.claims.get_ns(ns, name) else { return Ok(None) };
        let c = (*c).clone();
        // `syncClaim`: a claim is bound once the controller completed the binding.
        let bind_completed = c.metadata.annotations.as_ref().is_some_and(|a| a.contains_key(ANN_BIND_COMPLETED));
        if bind_completed {
            self.sync_bound_claim(c).await?;
        } else {
            self.sync_unbound_claim(c).await?;
        }
        Ok(None)
    }

    async fn sync_unbound_claim(&self, c: PersistentVolumeClaim) -> anyhow::Result<()> {
        let vn = claim_volume_name(&c).to_string();
        if vn.is_empty() {
            let delay = self.delay_binding(&c);
            let all = self.volumes.list();
            let refs: Vec<&PersistentVolume> = all.iter().map(|v| v.as_ref()).collect();
            if let Some(v) = find_best_match_for_claim(&c, &refs, delay) {
                return self.bind(v.clone(), c).await;
            }
            // `assignDefaultStorageClass` (retroactive default class).
            if claim_class_unset(&c) {
                let classes: Vec<Value> =
                    self.classes.list().iter().map(|s| serde_json::to_value(s.as_ref()).unwrap_or_default()).collect();
                if let Some(def) = kube_plugin_default_class(&classes) {
                    let mut c2 = c.clone();
                    set_path(&mut c2.spec, "storageClassName", json!(def));
                    self.pvc_api(c.metadata.namespace()).replace(&c2).await?;
                    return Ok(());
                }
            }
            let class = claim_class(&c);
            if delay && !c.metadata.annotations.as_ref().is_some_and(|a| a.contains_key(ANN_SELECTED_NODE)) {
                self.event_for_delay_binding(&c);
            } else if !class.is_empty() {
                return self.provision_claim(c).await;
            } else {
                self.recorder.event(
                    &c,
                    NORMAL,
                    "FailedBinding",
                    "no persistent volumes available for this claim and no storage class is set",
                );
            }
            self.update_claim_status(&c, "Pending", None).await?;
            return Ok(());
        }
        // Pre-bound claim.
        let Some(v) = self.volumes.get(&vn) else {
            self.update_claim_status(&c, "Pending", None).await?;
            return Ok(());
        };
        let v = (*v).clone();
        if claim_ref(&v).is_none() {
            if let Err(why) = check_volume_satisfies_claim(&v, &c) {
                self.recorder.event(&c, WARNING, "VolumeMismatch", format!("Cannot bind to requested volume {vn:?}: {why}"));
                self.update_claim_status(&c, "Pending", None).await?;
                return Ok(());
            }
            return self.bind(v, c).await;
        }
        if is_volume_bound_to_claim(&v, &c) {
            return self.bind(v, c).await;
        }
        self.recorder.event(&c, WARNING, "FailedBinding", format!("volume {vn:?} already bound to a different claim."));
        self.update_claim_status(&c, "Pending", None).await?;
        Ok(())
    }

    async fn sync_bound_claim(&self, c: PersistentVolumeClaim) -> anyhow::Result<()> {
        let vn = claim_volume_name(&c).to_string();
        if vn.is_empty() {
            self.claim_lost(&c, "ClaimLost", "Bound claim has lost reference to PersistentVolume. Data on the volume is lost!").await?;
            return Ok(());
        }
        let Some(v) = self.volumes.get(&vn) else {
            self.claim_lost(&c, "ClaimLost", "Bound claim has lost its PersistentVolume. Data on the volume is lost!").await?;
            return Ok(());
        };
        let v = (*v).clone();
        let uid_matches = claim_ref(&v).is_some_and(|r| r["uid"].as_str() == c.metadata.uid.as_deref());
        if claim_ref(&v).is_none() || uid_matches {
            return self.bind(v, c).await;
        }
        self.claim_lost(&c, "ClaimMisbound", "Two claims are bound to the same volume, this one is bound incorrectly").await?;
        Ok(())
    }

    async fn claim_lost(&self, c: &PersistentVolumeClaim, reason: &str, msg: &str) -> anyhow::Result<()> {
        if claim_phase(c) != "Lost" {
            self.recorder.event(c, WARNING, reason, msg);
        }
        self.update_claim_status(c, "Lost", None).await
    }

    /// `emitEventForUnboundDelayBindingClaim`
    fn event_for_delay_binding(&self, c: &PersistentVolumeClaim) {
        let name = c.metadata.name();
        let mut pods: Vec<String> = self
            .pods
            .list_namespace(c.metadata.namespace())
            .into_iter()
            .filter(|p| p.spec.as_ref().and_then(|s| s.node_name.as_deref()).is_none_or(str::is_empty))
            .filter(|p| pod_claim_names(p).iter().any(|n| n == name))
            .map(|p| p.metadata.name().to_string())
            .collect();
        pods.sort();
        let (reason, msg) = match pods.len() {
            0 => ("WaitForFirstConsumer", "waiting for first consumer to be created before binding".to_string()),
            1 => ("WaitForPodScheduled", format!("waiting for pod {} to be scheduled", pods[0])),
            _ => ("WaitForPodScheduled", format!("waiting for pods {} to be scheduled", pods.join(","))),
        };
        self.recorder.event(c, NORMAL, reason, msg);
    }

    /// `updateClaimStatus`: phase, and the volume's access modes and capacity when bound.
    async fn update_claim_status(&self, c: &PersistentVolumeClaim, phase: &str, v: Option<&PersistentVolume>) -> anyhow::Result<()> {
        let mut status = c.status.clone().unwrap_or_else(|| json!({}));
        status["phase"] = json!(phase);
        match v {
            Some(v) => {
                status["accessModes"] = json!(volume_access_modes(v));
                status["capacity"] = v.spec.as_ref().and_then(|s| s.get("capacity")).cloned().unwrap_or(json!({}));
            }
            None if phase != "Bound" => {
                if let Some(m) = status.as_object_mut() {
                    m.remove("accessModes");
                    m.remove("capacity");
                }
            }
            None => {}
        }
        if Some(&status) == c.status.as_ref() {
            return Ok(());
        }
        let mut c2 = c.clone();
        c2.status = Some(status);
        self.pvc_api(c.metadata.namespace()).replace_status(&c2).await?;
        Ok(())
    }

    /// `bind`: volume → claim (`claimRef`), volume Bound, claim → volume (`volumeName`), claim Bound.
    async fn bind(&self, v: PersistentVolume, c: PersistentVolumeClaim) -> anyhow::Result<()> {
        let (mut v, dirty) = bind_volume_to_claim(&v, &c);
        if dirty {
            v = self.pv_api().replace(&v).await?;
        }
        v = self.update_volume_phase(&v, "Bound", None).await?;
        let mut c2 = c.clone();
        let mut changed = false;
        if claim_volume_name(&c) != v.metadata.name() {
            set_path(&mut c2.spec, "volumeName", json!(v.metadata.name()));
            c2.metadata.annotations.get_or_insert_with(Default::default).insert(ANN_BOUND_BY_CONTROLLER.into(), "yes".into());
            changed = true;
        }
        if !c2.metadata.annotations.as_ref().is_some_and(|a| a.contains_key(ANN_BIND_COMPLETED)) {
            c2.metadata.annotations.get_or_insert_with(Default::default).insert(ANN_BIND_COMPLETED.into(), "yes".into());
            changed = true;
        }
        let c2 = if changed { self.pvc_api(c.metadata.namespace()).replace(&c2).await? } else { c2 };
        self.update_claim_status(&c2, "Bound", Some(&v)).await
    }

    async fn update_volume_phase(
        &self,
        v: &PersistentVolume,
        phase: &str,
        reason: Option<(&str, &str)>,
    ) -> anyhow::Result<PersistentVolume> {
        let mut status = v.status.clone().unwrap_or_else(|| json!({}));
        let unchanged = status["phase"] == phase;
        status["phase"] = json!(phase);
        match reason {
            Some((_, msg)) => {
                status["message"] = json!(msg);
            }
            None => {
                if let Some(m) = status.as_object_mut() {
                    m.remove("message");
                    m.remove("reason");
                }
            }
        }
        if !unchanged {
            status["lastPhaseTransitionTime"] = json!(k8s_apimachinery::meta::now_rfc3339());
        }
        if Some(&status) == v.status.as_ref() {
            return Ok(v.clone());
        }
        if let Some((r, msg)) = reason {
            self.recorder.event(v, WARNING, r, msg.to_string());
        }
        let mut v2 = v.clone();
        v2.status = Some(status);
        Ok(self.pv_api().replace_status(&v2).await?)
    }

    // -----------------------------------------------------------------------
    // provisioning
    // -----------------------------------------------------------------------

    /// `provisionClaim`: in-tree host-path, or ask the external provisioner.
    async fn provision_claim(&self, c: PersistentVolumeClaim) -> anyhow::Result<()> {
        let class_name = claim_class(&c);
        let Some(class) = self.class(&class_name) else {
            let msg = format!("storageclass.storage.k8s.io {class_name:?} not found");
            self.recorder.event(&c, WARNING, "ProvisioningFailed", msg);
            return self.update_claim_status(&c, "Pending", None).await;
        };
        if class.provisioner == NOT_SUPPORTED_PROVISIONER {
            self.recorder.event(&c, NORMAL, "FailedBinding", "no persistent volumes available for this claim and no storage class is set");
            return self.update_claim_status(&c, "Pending", None).await;
        }
        if class.provisioner == HOST_PATH_PLUGIN && self.hostpath_provisioner {
            return self.provision_host_path(&c, &class).await;
        }
        // External provisioner: annotate the claim and wait (`provisionClaimOperationExternal`).
        let p = class.provisioner.clone();
        let has = c.metadata.annotations.as_ref().is_some_and(|a| a.get(ANN_STORAGE_PROVISIONER) == Some(&p));
        let c = if !has {
            let mut c2 = c.clone();
            let a = c2.metadata.annotations.get_or_insert_with(Default::default);
            a.insert(ANN_STORAGE_PROVISIONER.into(), p.clone());
            a.insert(ANN_BETA_STORAGE_PROVISIONER.into(), p.clone());
            let saved = self.pvc_api(c.metadata.namespace()).replace(&c2).await?;
            self.recorder.event(
                &saved,
                NORMAL,
                "ExternalProvisioning",
                format!(
                    "Waiting for a volume to be created either by the external provisioner '{p}' or manually by the system administrator. If volume creation is delayed, please verify that the provisioner is running and correctly registered."
                ),
            );
            saved
        } else {
            c
        };
        self.update_claim_status(&c, "Pending", None).await
    }

    /// `hostPathProvisioner.Provision` + `provisionClaimOperation`.
    async fn provision_host_path(&self, c: &PersistentVolumeClaim, class: &StorageClass) -> anyhow::Result<()> {
        let pv_name = format!("pvc-{}", c.metadata.uid.as_deref().unwrap_or(""));
        if self.volumes.get(&pv_name).is_some() || self.pv_api().get_opt(&pv_name).await?.is_some() {
            return Ok(()); // already provisioned; binding follows
        }
        if c.spec.as_ref().and_then(|s| s["volumeMode"].as_str()) == Some("Block") {
            let msg = format!("{HOST_PATH_PLUGIN} does not support block volume provisioning");
            self.recorder.event(c, WARNING, "ProvisioningFailed", msg);
            return Ok(());
        }
        let path = format!("{HOST_PATH_BASE}/{}", uuid::Uuid::new_v4());
        if let Err(e) = std::fs::create_dir_all(&path).and_then(|_| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o750))
        }) {
            self.recorder.event(
                c,
                WARNING,
                "ProvisioningFailed",
                format!("Failed to provision volume with StorageClass {:?}: {e}", class.metadata.name()),
            );
            return Ok(());
        }
        let modes = claim_access_modes(c);
        let modes = if modes.is_empty() { vec!["ReadWriteOnce".to_string()] } else { modes };
        let capacity = c.spec.as_ref().and_then(|s| s.pointer("/resources/requests/storage")).cloned().unwrap_or(json!("0"));
        let pv: PersistentVolume = serde_json::from_value(json!({
            "apiVersion": "v1", "kind": "PersistentVolume",
            "metadata": {"name": pv_name, "annotations": {
                "kubernetes.io/createdby": "hostpath-dynamic-provisioner",
                ANN_BOUND_BY_CONTROLLER: "yes",
                ANN_DYNAMICALLY_PROVISIONED: HOST_PATH_PLUGIN,
            }},
            "spec": {
                "persistentVolumeReclaimPolicy": class.reclaim_policy.clone().unwrap_or_else(|| "Delete".into()),
                "accessModes": modes,
                "capacity": {"storage": capacity},
                "hostPath": {"path": path},
                "storageClassName": class.metadata.name(),
                "mountOptions": class.mount_options,
                "claimRef": {"kind": "PersistentVolumeClaim", "apiVersion": "v1", "namespace": c.metadata.namespace(),
                             "name": c.metadata.name(), "uid": c.metadata.uid, "resourceVersion": c.metadata.resource_version},
            },
        }))?;
        match self.pv_api().create(&pv).await {
            Ok(_) => {
                self.recorder.event(
                    c,
                    NORMAL,
                    "ProvisioningSucceeded",
                    format!("Successfully provisioned volume {pv_name} using {HOST_PATH_PLUGIN}"),
                );
                Ok(())
            }
            Err(e) if e.is_already_exists() => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&path);
                self.recorder.event(
                    c,
                    WARNING,
                    "ProvisioningFailed",
                    format!("Failed to provision volume with StorageClass {:?}: {e}", class.metadata.name()),
                );
                Err(e.into())
            }
        }
    }

    // -----------------------------------------------------------------------
    // volumes
    // -----------------------------------------------------------------------

    async fn sync_volume_key(&self, k: &str) -> SyncResult {
        let Some(v) = self.volumes.get(k) else { return Ok(None) };
        self.sync_volume((*v).clone()).await?;
        Ok(None)
    }

    /// `syncVolume`
    async fn sync_volume(&self, v: PersistentVolume) -> anyhow::Result<()> {
        let Some(r) = claim_ref(&v).cloned() else {
            self.update_volume_phase(&v, "Available", None).await?;
            return Ok(());
        };
        if r["uid"].as_str().unwrap_or("").is_empty() {
            // Pre-bound to a claim name only: the claim sync finishes the binding.
            self.update_volume_phase(&v, "Available", None).await?;
            return Ok(());
        }
        let (ns, name) = (r["namespace"].as_str().unwrap_or(""), r["name"].as_str().unwrap_or(""));
        let mut claim = self.claims.get_ns(ns, name).map(|c| (*c).clone());
        if claim.is_none() && !matches!(volume_phase(&v), "Released" | "Failed") {
            claim = self.pvc_api(ns).get_opt(name).await?;
        }
        let claim = claim.filter(|c| c.metadata.uid.as_deref() == r["uid"].as_str());
        let Some(claim) = claim else {
            let v =
                if !matches!(volume_phase(&v), "Released" | "Failed") { self.update_volume_phase(&v, "Released", None).await? } else { v };
            return self.reclaim(v).await;
        };
        let vn = claim_volume_name(&claim);
        if vn.is_empty() {
            if volume_mode_mismatch(&claim, &v) {
                self.recorder.event(
                    &v,
                    WARNING,
                    "VolumeMismatch",
                    format!(
                        "Cannot bind PersistentVolume to requested PersistentVolumeClaim {:?} due to incompatible volumeMode.",
                        claim.metadata.name()
                    ),
                );
                return Ok(());
            }
            self.claim_queue.add(key(&claim));
            return Ok(());
        }
        if vn == v.metadata.name() {
            self.update_volume_phase(&v, "Bound", None).await?;
            return Ok(());
        }
        // The claim is bound to another volume.
        let provisioned = v.metadata.annotations.as_ref().is_some_and(|a| a.contains_key(ANN_DYNAMICALLY_PROVISIONED));
        let policy = v.spec.as_ref().and_then(|s| s["persistentVolumeReclaimPolicy"].as_str()).unwrap_or("Retain");
        if provisioned && policy == "Delete" {
            let v =
                if !matches!(volume_phase(&v), "Released" | "Failed") { self.update_volume_phase(&v, "Released", None).await? } else { v };
            return self.reclaim(v).await;
        }
        self.unbind_volume(v).await
    }

    /// `unbindVolume`
    async fn unbind_volume(&self, v: PersistentVolume) -> anyhow::Result<()> {
        let mut v2 = v.clone();
        let by_controller = v.metadata.annotations.as_ref().is_some_and(|a| a.contains_key(ANN_BOUND_BY_CONTROLLER));
        if by_controller {
            if let Some(s) = v2.spec.as_mut().and_then(Value::as_object_mut) {
                s.remove("claimRef");
            }
            if let Some(a) = v2.metadata.annotations.as_mut() {
                a.remove(ANN_BOUND_BY_CONTROLLER);
            }
        } else if let Some(r) = v2.spec.as_mut().and_then(|s| s.get_mut("claimRef")) {
            r["uid"] = json!("");
        }
        let v2 = self.pv_api().replace(&v2).await?;
        self.update_volume_phase(&v2, "Available", None).await?;
        Ok(())
    }

    /// `reclaimVolume`
    async fn reclaim(&self, v: PersistentVolume) -> anyhow::Result<()> {
        match v.spec.as_ref().and_then(|s| s["persistentVolumeReclaimPolicy"].as_str()).unwrap_or("Retain") {
            "Retain" => Ok(()),
            "Recycle" => {
                if volume_phase(&v) != "Failed" {
                    self.update_volume_phase(&v, "Failed", Some(("VolumeFailedRecycle", "No recycler plugin found for the volume!")))
                        .await?;
                }
                Ok(())
            }
            _ => self.delete_volume(v).await,
        }
    }

    /// `deleteVolumeOperation`: host-path volumes under `/tmp` are removed here;
    /// volumes of external provisioners are left to them.
    async fn delete_volume(&self, v: PersistentVolume) -> anyhow::Result<()> {
        if volume_phase(&v) == "Failed" {
            return Ok(());
        }
        let provisioner = v.metadata.annotations.as_ref().and_then(|a| a.get(ANN_DYNAMICALLY_PROVISIONED)).cloned().unwrap_or_default();
        let host_path = v.spec.as_ref().and_then(|s| s.pointer("/hostPath/path")).and_then(Value::as_str).map(String::from);
        if !provisioner.is_empty() && provisioner != HOST_PATH_PLUGIN {
            return Ok(()); // the external provisioner deletes it
        }
        let Some(path) = host_path else {
            let msg = format!("error getting deleter volume plugin for volume {:?}: no deletable volume plugin matched", v.metadata.name());
            self.update_volume_phase(&v, "Failed", Some(("VolumeFailedDelete", &msg))).await?;
            return Ok(());
        };
        // `hostPathDeleter.Delete`
        let ok_path = path.starts_with("/tmp/") && path.len() > "/tmp/".len();
        if !ok_path {
            let msg = format!("host_path deleter only supports /tmp/.+ but received provided {path}");
            self.update_volume_phase(&v, "Failed", Some(("VolumeFailedDelete", &msg))).await?;
            return Ok(());
        }
        if let Err(e) = std::fs::remove_dir_all(&path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                let msg = format!("unlinkat {path}: {}", e.to_string().split(" (os error").next().unwrap_or("").to_lowercase());
                self.update_volume_phase(&v, "Failed", Some(("VolumeFailedDelete", &msg))).await?;
                return Ok(());
            }
        }
        match self.pv_api().delete(v.metadata.name()).await {
            Ok(_) => Ok(()),
            Err(e) if e.is_not_found() => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// `checkVolumeSatisfyClaim`
pub fn check_volume_satisfies_claim(v: &PersistentVolume, c: &PersistentVolumeClaim) -> Result<(), String> {
    if volume_capacity(v) < claim_request(c) {
        return Err("requested PV is too small".into());
    }
    if volume_class(v) != claim_class(c) {
        return Err("storageClassName does not match".into());
    }
    if volume_mode_mismatch(c, v) {
        return Err("incompatible volumeMode".into());
    }
    if !check_access_modes(c, v) {
        return Err("incompatible accessMode".into());
    }
    Ok(())
}

/// The default class, chosen as the `DefaultStorageClass` admission plugin does.
fn kube_plugin_default_class(classes: &[Value]) -> Option<String> {
    let mut d: Vec<&Value> = classes
        .iter()
        .filter(|c| {
            ["storageclass.kubernetes.io~1is-default-class", "storageclass.beta.kubernetes.io~1is-default-class"]
                .iter()
                .any(|a| c.pointer(&format!("/metadata/annotations/{a}")).and_then(Value::as_str) == Some("true"))
        })
        .collect();
    let ts = |c: &Value| c.pointer("/metadata/creationTimestamp").and_then(Value::as_str).unwrap_or("").to_string();
    d.sort_by_key(|c| std::cmp::Reverse(ts(c)));
    d.first().and_then(|c| c.pointer("/metadata/name")).and_then(Value::as_str).map(String::from)
}
