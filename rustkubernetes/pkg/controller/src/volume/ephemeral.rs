//! Port of `pkg/controller/volume/ephemeral` (`ephemeral-volume-controller`):
//! every generic ephemeral volume of a pod gets a claim `<pod>-<volume>` made
//! from its `volumeClaimTemplate`, owned by the pod (deleted with it).

use std::sync::Arc;

use k8s_api::core::v1::{PersistentVolumeClaim, Pod};
use k8s_client_go::informers::SharedInformerFactory;
use k8s_client_go::tools::cache::{split_key, Store};
use k8s_client_go::tools::record::{EventRecorder, WARNING};
use k8s_client_go::util::workqueue::WorkQueue;
use k8s_client_go::{Api, Client};
use serde_json::{json, Value};

use crate::controller_utils::*;
use crate::ControllerContext;

pub struct Controller {
    client: Client,
    pods: Store<Pod>,
    claims: Store<PersistentVolumeClaim>,
    informers: Arc<SharedInformerFactory>,
    queue: Queue,
    recorder: EventRecorder,
}

impl Controller {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let pod_inf = ctx.informers.informer::<Pod>();
        let pvc_inf = ctx.informers.informer::<PersistentVolumeClaim>();
        let c = Arc::new(Self {
            client: ctx.client.clone(),
            pods: pod_inf.store(),
            claims: pvc_inf.store(),
            informers: ctx.informers.clone(),
            queue: WorkQueue::new(),
            recorder: ctx.recorder("ephemeral_volume"),
        });
        let me = c.clone();
        on_events(pod_inf.subscribe(), move |ev| me.queue.add(key(ev.object().as_ref())));
        c
    }

    pub async fn run(self: Arc<Self>, workers: usize) {
        self.informers.wait_for_cache_sync().await;
        let me = self.clone();
        run_workers("ephemeral-volume-controller", self.queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.sync(&k).await }
        })
        .await;
    }

    async fn sync(&self, k: &str) -> SyncResult {
        let (ns, name) = split_key(k);
        let Some(pod) = self.pods.get_ns(ns, name) else { return Ok(None) };
        if pod.metadata.deletion_timestamp.is_some() {
            return Ok(None);
        }
        let api: Api<PersistentVolumeClaim> = Api::namespaced(self.client.clone(), ns);
        for v in pod.spec.as_ref().and_then(|s| s.volumes.clone()).unwrap_or_default() {
            let Some(eph) = v.source.get("ephemeral").filter(|e| e.is_object()) else { continue };
            let claim_name = format!("{name}-{}", v.name);
            if let Some(existing) = self.claims.get_ns(ns, &claim_name) {
                let owned = existing.metadata.owner_references.iter().flatten().any(|o| Some(&o.uid) == pod.metadata.uid.as_ref());
                if !owned {
                    self.recorder.event(
                        pod.as_ref(),
                        WARNING,
                        "FailedBinding",
                        format!("PVC {ns}/{claim_name} was not created for pod {ns}/{name} (pod is not owner)"),
                    );
                }
                continue;
            }
            let template = eph.get("volumeClaimTemplate").cloned().unwrap_or(json!({}));
            let mut meta = template.get("metadata").cloned().unwrap_or(json!({}));
            meta["name"] = json!(claim_name);
            meta["namespace"] = json!(ns);
            meta["ownerReferences"] = json!([{
                "apiVersion": "v1", "kind": "Pod", "name": name, "uid": pod.metadata.uid,
                "controller": true, "blockOwnerDeletion": true,
            }]);
            let obj: Value = json!({"apiVersion": "v1", "kind": "PersistentVolumeClaim", "metadata": meta, "spec": template.get("spec").cloned().unwrap_or(json!({}))});
            let pvc: PersistentVolumeClaim = serde_json::from_value(obj)?;
            match api.create(&pvc).await {
                Ok(_) => {}
                Err(e) if e.is_already_exists() => {}
                Err(e) => {
                    self.recorder.event(pod.as_ref(), WARNING, "FailedBinding", format!("create PVC {claim_name}: {e}"));
                    return Err(e.into());
                }
            }
        }
        Ok(None)
    }
}
