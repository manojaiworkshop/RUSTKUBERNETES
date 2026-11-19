//! Persistent volume controllers against a real in-process API server
//! (Go: `test/integration/volume/persistent_volumes_test.go`).

use std::time::Duration;

use k8s_api::core::v1::{Event, PersistentVolume, PersistentVolumeClaim};
use k8s_client_go::{Api, ListParams};
use kube_controller::{controller_descriptors, ControllerContext};
use kube_integration_tests::{eventually, start_test_server, test_controller_config};
use serde_json::{json, Value};

const T: Duration = Duration::from_secs(20);

fn from<T: serde::de::DeserializeOwned>(v: Value) -> T {
    serde_json::from_value(v).unwrap()
}

fn s(v: &Option<Value>, p: &str) -> String {
    v.as_ref().and_then(|v| v.pointer(p)).and_then(Value::as_str).unwrap_or("").to_string()
}

#[tokio::test]
async fn bind_provision_release_reclaim() {
    let srv = start_test_server().await;
    let mut cfg = test_controller_config();
    cfg.volume.enable_hostpath_provisioner = true;
    cfg.volume.pv_claim_binder_sync_period = Duration::from_secs(1);
    let ctx = ControllerContext::new(srv.client.clone(), cfg);
    let mut runs = vec![];
    for d in controller_descriptors() {
        if ["persistentvolume-binder-controller", "persistentvolumeclaim-protection-controller", "persistentvolume-protection-controller"]
            .contains(&d.name)
        {
            runs.push((d.constructor)(&ctx));
        }
    }
    ctx.informers.start();
    for r in runs {
        tokio::spawn(r);
    }
    let pvs: Api<PersistentVolume> = Api::all(srv.client.clone());
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(srv.client.clone(), "default");

    // Static binding: the smallest volume that fits.
    for (name, size) in [("big", "10Gi"), ("small", "2Gi")] {
        pvs.create(&from(json!({"metadata": {"name": name}, "spec": {"capacity": {"storage": size}, "accessModes": ["ReadWriteOnce"],
            "persistentVolumeReclaimPolicy": "Retain", "hostPath": {"path": format!("/srv/{name}")}}})))
            .await
            .unwrap();
    }
    pvcs.create(&from(json!({"metadata": {"name": "data"}, "spec": {"accessModes": ["ReadWriteOnce"], "storageClassName": "",
        "resources": {"requests": {"storage": "1Gi"}}}})))
        .await
        .unwrap();
    let c = eventually("claim bound", T, || async { pvcs.get("data").await.ok().filter(|c| s(&c.status, "/phase") == "Bound") }).await;
    assert_eq!(s(&c.spec, "/volumeName"), "small");
    assert_eq!(s(&c.status, "/capacity/storage"), "2Gi");
    let ann = c.metadata.annotations.clone().unwrap();
    assert_eq!(ann["pv.kubernetes.io/bind-completed"], "yes");
    assert!(c.metadata.finalizers.unwrap().contains(&"kubernetes.io/pvc-protection".to_string()));
    let v = eventually("volume bound", T, || async { pvs.get("small").await.ok().filter(|v| s(&v.status, "/phase") == "Bound") }).await;
    assert_eq!(s(&v.spec, "/claimRef/name"), "data");

    // Releasing a Retain volume.
    pvcs.delete("data").await.unwrap();
    eventually("volume released", T, || async { pvs.get("small").await.ok().filter(|v| s(&v.status, "/phase") == "Released") }).await;
    eventually("claim gone", T, || async { pvcs.get_opt("data").await.ok().filter(Option::is_none) }).await;

    // No class, no volume: Pending with the upstream event.
    pvcs.create(&from(json!({"metadata": {"name": "huge"}, "spec": {"accessModes": ["ReadWriteOnce"], "storageClassName": "",
        "resources": {"requests": {"storage": "100Gi"}}}})))
        .await
        .unwrap();
    let events: Api<Event> = Api::namespaced(srv.client.clone(), "default");
    eventually("FailedBinding event", T, || async {
        events.list(&ListParams::default()).await.ok()?.items.into_iter().find(|e| {
            e.reason.as_deref() == Some("FailedBinding")
                && e.message.as_deref() == Some("no persistent volumes available for this claim and no storage class is set")
        })
    })
    .await;

    // WaitForFirstConsumer: no binding until a node is selected.
    let classes: Api<k8s_api::storage::v1::StorageClass> = Api::all(srv.client.clone());
    classes.create(&from(json!({"metadata": {"name": "local"}, "provisioner": "kubernetes.io/no-provisioner", "volumeBindingMode": "WaitForFirstConsumer"}))).await.unwrap();
    pvs.create(&from(json!({"metadata": {"name": "local-pv"}, "spec": {"capacity": {"storage": "1Gi"}, "accessModes": ["ReadWriteOnce"],
        "storageClassName": "local", "local": {"path": "/mnt/disk"}, "nodeAffinity": {"required": {"nodeSelectorTerms": [{"matchExpressions": [
            {"key": "kubernetes.io/hostname", "operator": "In", "values": ["n1"]}]}]}}}}))).await.unwrap();
    pvcs.create(&from(json!({"metadata": {"name": "later"}, "spec": {"accessModes": ["ReadWriteOnce"], "storageClassName": "local",
        "resources": {"requests": {"storage": "1Gi"}}}})))
        .await
        .unwrap();
    eventually("WaitForFirstConsumer event", T, || async {
        events.list(&ListParams::default()).await.ok()?.items.into_iter().find(|e| e.reason.as_deref() == Some("WaitForFirstConsumer"))
    })
    .await;
    assert_eq!(s(&pvcs.get("later").await.unwrap().status, "/phase"), "Pending");

    // Dynamic provisioning with the in-tree host-path provisioner and a default class.
    classes
        .create(&from(json!({"metadata": {"name": "standard", "annotations": {"storageclass.kubernetes.io/is-default-class": "true"}},
        "provisioner": "kubernetes.io/host-path"})))
        .await
        .unwrap();
    pvcs.create(&from(
        json!({"metadata": {"name": "dyn"}, "spec": {"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "1Mi"}}}}),
    ))
    .await
    .unwrap();
    let dyn_claim =
        eventually("provisioned and bound", T, || async { pvcs.get("dyn").await.ok().filter(|c| s(&c.status, "/phase") == "Bound") }).await;
    assert_eq!(s(&dyn_claim.spec, "/storageClassName"), "standard", "DefaultStorageClass admission");
    let pv_name = s(&dyn_claim.spec, "/volumeName");
    assert_eq!(pv_name, format!("pvc-{}", dyn_claim.metadata.uid.clone().unwrap()));
    let pv = pvs.get(&pv_name).await.unwrap();
    let dir = s(&pv.spec, "/hostPath/path");
    assert!(dir.starts_with("/tmp/hostpath_pv/") && std::path::Path::new(&dir).is_dir());
    assert_eq!(pv.metadata.annotations.clone().unwrap()["pv.kubernetes.io/provisioned-by"], "kubernetes.io/host-path");
    assert_eq!(s(&pv.spec, "/persistentVolumeReclaimPolicy"), "Delete");
    // Delete policy: claim gone → volume and directory gone.
    pvcs.delete("dyn").await.unwrap();
    eventually("provisioned volume deleted", T, || async { pvs.get_opt(&pv_name).await.ok().filter(Option::is_none) }).await;
    assert!(!std::path::Path::new(&dir).exists());
}
