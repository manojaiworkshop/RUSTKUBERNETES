//! End-to-end API server behaviour, exercised through the Rust client-go.

use std::time::Duration;

use futures::StreamExt;
use k8s_api::apps::v1::Deployment;
use k8s_api::core::v1::{ConfigMap, Namespace, Pod, Service};
use k8s_apimachinery::meta::ObjectMeta;
use k8s_apimachinery::watch::EventType;
use k8s_client_go::api::{ApiResource, DynamicApi};
use k8s_client_go::tools::cache::{Event, Informer};
use k8s_client_go::{Api, ListParams, PatchType};
use kube_integration_tests::start_test_server;
use serde_json::json;

fn pod(name: &str, labels: &[(&str, &str)]) -> Pod {
    serde_json::from_value(json!({
        "metadata": {"name": name, "labels": labels.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<serde_json::Map<_, _>>()},
        "spec": {"containers": [{"name": "app", "image": "nginx:1.27"}]},
    }))
    .unwrap()
}

#[tokio::test]
async fn bootstrap_objects_exist() {
    let ts = start_test_server().await;
    let ns: Api<Namespace> = Api::all(ts.client.clone());
    let names: Vec<String> = ns.list(&ListParams::default()).await.unwrap().items.iter().map(|n| n.metadata.name().to_string()).collect();
    assert_eq!(names, ["default", "kube-node-lease", "kube-public", "kube-system"]);
    let svc: Api<Service> = Api::namespaced(ts.client.clone(), "default");
    let k = svc.get("kubernetes").await.unwrap();
    assert_eq!(k.spec.unwrap().cluster_ip.as_deref(), Some("10.96.0.1"));
}

#[tokio::test]
async fn pod_crud_defaults_and_validation() {
    let ts = start_test_server().await;
    let pods: Api<Pod> = Api::namespaced(ts.client.clone(), "default");

    let created = pods.create(&pod("web", &[("app", "web")])).await.unwrap();
    let spec = created.spec.clone().unwrap();
    assert_eq!(spec.restart_policy.as_deref(), Some("Always"));
    assert_eq!(spec.containers[0].image_pull_policy.as_deref(), Some("IfNotPresent"));
    assert_eq!(created.status.as_ref().unwrap().phase.as_deref(), Some("Pending"));
    assert!(created.metadata.uid.is_some());

    // label selectors
    pods.create(&pod("db", &[("app", "db")])).await.unwrap();
    let web = pods.list(&ListParams::default().labels("app=web")).await.unwrap();
    assert_eq!(web.items.len(), 1);
    let not_web = pods.list(&ListParams::default().labels("app!=web")).await.unwrap();
    assert_eq!(not_web.items[0].metadata.name(), "db");

    // mutable field: image
    let mut p = created.clone();
    p.spec.as_mut().unwrap().containers[0].image = Some("nginx:1.28".into());
    let updated = pods.replace(&p).await.unwrap();
    assert_eq!(updated.spec.unwrap().containers[0].image.as_deref(), Some("nginx:1.28"));

    // stale resourceVersion → 409 Conflict
    let err = pods.replace(&p).await.unwrap_err();
    assert!(err.is_conflict(), "{err}");

    // immutable field → 422
    let mut p = pods.get("web").await.unwrap();
    p.spec.as_mut().unwrap().node_name = Some("n1".into());
    let err = pods.replace(&p).await.unwrap_err();
    assert!(err.to_string().contains("pod updates may not change fields"), "{err}");

    // invalid create → 422 with causes
    let bad: Pod = serde_json::from_value(json!({"metadata": {"name": "Bad"}, "spec": {"containers": []}})).unwrap();
    let err = pods.create(&bad).await.unwrap_err();
    let k8s_client_go::Error::Api(api) = &err else { panic!("{err}") };
    assert_eq!(api.code, 422);
    assert_eq!(api.details.as_ref().unwrap().causes.as_ref().unwrap().len(), 2);

    // status subresource ignores spec changes, main endpoint ignores status
    let mut p = pods.get("web").await.unwrap();
    p.status.as_mut().unwrap().phase = Some("Running".into());
    let after = pods.replace(&p).await.unwrap();
    assert_eq!(after.status.unwrap().phase.as_deref(), Some("Pending"));
    let mut p = pods.get("web").await.unwrap();
    p.status.as_mut().unwrap().phase = Some("Running".into());
    let after = pods.replace_status(&p).await.unwrap();
    assert_eq!(after.status.unwrap().phase.as_deref(), Some("Running"));

    // unscheduled pods are deleted immediately
    pods.delete("db").await.unwrap();
    assert!(pods.get("db").await.unwrap_err().is_not_found());
}

#[tokio::test]
async fn binding_and_graceful_deletion() {
    let ts = start_test_server().await;
    let pods: Api<Pod> = Api::namespaced(ts.client.clone(), "default");
    pods.create(&pod("p", &[])).await.unwrap();
    let binding = json!({"apiVersion": "v1", "kind": "Binding", "metadata": {"name": "p"}, "target": {"kind": "Node", "name": "node-1"}});
    pods.dynamic().create_subresource("p", "binding", &binding).await.unwrap();
    let p = pods.get("p").await.unwrap();
    assert_eq!(p.spec.as_ref().unwrap().node_name.as_deref(), Some("node-1"));
    let scheduled = p.status.unwrap().conditions.unwrap();
    assert!(scheduled.iter().any(|c| c.type_ == "PodScheduled" && c.status == "True"));
    // second binding conflicts
    let err = pods.dynamic().create_subresource("p", "binding", &binding).await.unwrap_err();
    assert!(err.is_conflict() || err.to_string().contains("already assigned"), "{err}");

    // bound pod: graceful deletion marks it, kubelet's grace=0 delete removes it
    pods.delete("p").await.unwrap();
    let p = pods.get("p").await.unwrap();
    assert!(p.metadata.deletion_timestamp.is_some());
    assert_eq!(p.metadata.deletion_grace_period_seconds, Some(30));
    pods.dynamic().delete("p", Some(0)).await.unwrap();
    assert!(pods.get("p").await.unwrap_err().is_not_found());
}

#[tokio::test]
async fn services_allocate_and_release_ips() {
    let ts = start_test_server().await;
    let svcs: Api<Service> = Api::namespaced(ts.client.clone(), "default");
    let mk = |name: &str, extra: serde_json::Value| -> Service {
        let mut v = json!({"metadata": {"name": name}, "spec": {"ports": [{"port": 80}], "selector": {"app": name}}});
        json_merge(&mut v, extra);
        serde_json::from_value(v).unwrap()
    };
    let a = svcs.create(&mk("a", json!({}))).await.unwrap();
    let b = svcs.create(&mk("b", json!({"spec": {"type": "NodePort"}}))).await.unwrap();
    let ip_a = a.spec.unwrap().cluster_ip.unwrap();
    let b_spec = b.spec.unwrap();
    assert_ne!(ip_a, b_spec.cluster_ip.clone().unwrap());
    let np = b_spec.ports.unwrap()[0].node_port.unwrap();
    assert!((30000..=32767).contains(&np));

    // requesting an allocated IP fails
    let err = svcs.create(&mk("c", json!({"spec": {"clusterIP": ip_a.clone()}}))).await.unwrap_err();
    assert!(err.to_string().contains("already allocated"), "{err}");
    // after delete the IP is free again
    svcs.delete("a").await.unwrap();
    let c = svcs.create(&mk("c", json!({"spec": {"clusterIP": ip_a.clone()}}))).await.unwrap();
    assert_eq!(c.spec.unwrap().cluster_ip.unwrap(), ip_a);
    // clusterIP is immutable but omitted values are preserved on update
    let mut c = svcs.get("c").await.unwrap();
    c.spec.as_mut().unwrap().cluster_ip = None;
    let c = svcs.replace(&c).await.unwrap();
    assert_eq!(c.spec.unwrap().cluster_ip.unwrap(), ip_a);
}

fn json_merge(a: &mut serde_json::Value, b: serde_json::Value) {
    match (a, b) {
        (serde_json::Value::Object(a), serde_json::Value::Object(b)) => {
            for (k, v) in b {
                json_merge(a.entry(k).or_insert(serde_json::Value::Null), v);
            }
        }
        (a, b) => *a = b,
    }
}

#[tokio::test]
async fn deployments_scale_and_generation() {
    let ts = start_test_server().await;
    let deploys: Api<Deployment> = Api::namespaced(ts.client.clone(), "default");
    let d: Deployment = serde_json::from_value(json!({
        "metadata": {"name": "web"},
        "spec": {"selector": {"matchLabels": {"app": "web"}},
                 "template": {"metadata": {"labels": {"app": "web"}},
                              "spec": {"containers": [{"name": "c", "image": "nginx"}]}}}
    }))
    .unwrap();
    let d = deploys.create(&d).await.unwrap();
    assert_eq!(d.metadata.generation, Some(1));
    assert_eq!(d.spec.as_ref().unwrap().replicas, Some(1));
    deploys.dynamic().patch_sub("web", Some("scale"), PatchType::Merge, &json!({"spec": {"replicas": 4}})).await.unwrap();
    let d = deploys.get("web").await.unwrap();
    assert_eq!(d.spec.unwrap().replicas, Some(4));
    assert_eq!(d.metadata.generation, Some(2));
    // selector does not match template → 422
    let bad: Deployment = serde_json::from_value(json!({
        "metadata": {"name": "bad"},
        "spec": {"selector": {"matchLabels": {"app": "x"}},
                 "template": {"metadata": {"labels": {"app": "y"}},
                              "spec": {"containers": [{"name": "c", "image": "nginx"}]}}}
    }))
    .unwrap();
    let err = deploys.create(&bad).await.unwrap_err();
    assert!(err.to_string().contains("`selector` does not match template `labels`"), "{err}");
}

#[tokio::test]
async fn namespace_lifecycle() {
    let ts = start_test_server().await;
    let cms: Api<ConfigMap> = Api::namespaced(ts.client.clone(), "team-a");
    let cm = ConfigMap::new(ObjectMeta::named("cfg"));
    assert!(cms.create(&cm).await.unwrap_err().is_not_found());

    let nss: Api<Namespace> = Api::all(ts.client.clone());
    nss.create(&Namespace::new(ObjectMeta::named("team-a"))).await.unwrap();
    cms.create(&cm).await.unwrap();
    let err = nss.delete("kube-system").await.unwrap_err();
    assert!(err.to_string().contains("may not be deleted"), "{err}");
    // Deleting marks the namespace Terminating; content stays until the namespace controller runs.
    nss.delete("team-a").await.unwrap();
    let ns = nss.get("team-a").await.unwrap();
    assert_eq!(ns.status.unwrap().phase.as_deref(), Some("Terminating"));
    assert!(ns.metadata.deletion_timestamp.is_some());
    assert!(cms.get("cfg").await.is_ok());
    let err = cms.create(&ConfigMap::new(ObjectMeta::named("late"))).await.unwrap_err();
    assert!(err.to_string().contains("being terminated"), "{err}");
    // What the namespace controller does: delete content, then clear spec.finalizers via /finalize.
    cms.delete("cfg").await.unwrap();
    let mut ns = serde_json::to_value(nss.get("team-a").await.unwrap()).unwrap();
    ns["spec"]["finalizers"] = json!([]);
    nss.dynamic().replace_sub("team-a", "finalize", &ns).await.unwrap_err_or_gone();
    assert!(nss.get("team-a").await.unwrap_err().is_not_found());
}

#[tokio::test]
async fn finalizers_block_deletion() {
    let ts = start_test_server().await;
    let cms: Api<ConfigMap> = Api::namespaced(ts.client.clone(), "default");
    let mut cm = ConfigMap::new(ObjectMeta::named("held"));
    cm.metadata.finalizers = Some(vec!["example.com/hold".into()]);
    cms.create(&cm).await.unwrap();
    cms.delete("held").await.unwrap();
    let held = cms.get("held").await.unwrap();
    assert!(held.metadata.deletion_timestamp.is_some());
    cms.patch("held", PatchType::Merge, &json!({"metadata": {"finalizers": null}})).await.unwrap_err_or_gone();
    assert!(cms.get("held").await.unwrap_err().is_not_found());
}

/// Removing the last finalizer deletes the object; the PATCH may return the deleted object.
trait PatchResult {
    fn unwrap_err_or_gone(self);
}
impl<T> PatchResult for Result<T, k8s_client_go::Error> {
    fn unwrap_err_or_gone(self) {
        if let Err(e) = self {
            assert!(e.is_not_found(), "{e}");
        }
    }
}

#[tokio::test]
async fn watch_with_selector_transitions() {
    let ts = start_test_server().await;
    let pods: Api<Pod> = Api::namespaced(ts.client.clone(), "default");
    let rv = pods.list(&ListParams::default()).await.unwrap().metadata.resource_version.unwrap();
    let mut lp = ListParams::default().labels("tier=web");
    lp.resource_version = Some(rv);
    let mut w = Box::pin(pods.watch(&lp).await.unwrap());

    pods.create(&pod("x", &[("tier", "db")])).await.unwrap(); // not matching: no event
    pods.create(&pod("y", &[("tier", "web")])).await.unwrap(); // ADDED
    pods.patch("x", PatchType::Merge, &json!({"metadata": {"labels": {"tier": "web"}}})).await.unwrap(); // enters selector: ADDED
    pods.patch("y", PatchType::Merge, &json!({"metadata": {"labels": {"tier": "db"}}})).await.unwrap(); // leaves: DELETED

    let mut seen = vec![];
    while seen.len() < 3 {
        let ev = tokio::time::timeout(Duration::from_secs(5), w.next()).await.unwrap().unwrap().unwrap();
        if ev.type_ != EventType::Bookmark {
            seen.push((ev.type_, ev.object.metadata.name().to_string()));
        }
    }
    assert_eq!(seen, [(EventType::Added, "y".into()), (EventType::Added, "x".into()), (EventType::Deleted, "y".into())]);
}

#[tokio::test]
async fn informer_tracks_changes() {
    let ts = start_test_server().await;
    let cms: Api<ConfigMap> = Api::namespaced(ts.client.clone(), "default");
    cms.create(&ConfigMap::new(ObjectMeta::named("before"))).await.unwrap();

    let informer = Informer::new(cms.clone(), ListParams::default());
    let mut events = informer.subscribe();
    informer.start();
    informer.wait_synced().await;
    assert!(informer.store().get("default/before").is_some());

    cms.create(&ConfigMap::new(ObjectMeta::named("after"))).await.unwrap();
    cms.patch("after", PatchType::Merge, &json!({"data": {"k": "v"}})).await.unwrap();
    cms.delete("before").await.unwrap();

    let mut kinds = vec![];
    while kinds.len() < 4 {
        let ev = tokio::time::timeout(Duration::from_secs(5), events.recv()).await.unwrap().unwrap();
        kinds.push(match ev {
            Event::Added(o) => format!("add:{}", o.metadata.name()),
            Event::Updated { new, .. } => format!("update:{}", new.metadata.name()),
            Event::Deleted(o) => format!("delete:{}", o.metadata.name()),
        });
    }
    assert_eq!(kinds, ["add:before", "add:after", "update:after", "delete:before"]);
    let store = informer.store();
    assert!(store.get("default/before").is_none());
    assert_eq!(store.get("default/after").unwrap().data.as_ref().unwrap()["k"], "v");
}

#[tokio::test]
async fn pagination_and_dynamic_client() {
    let ts = start_test_server().await;
    let cm =
        ApiResource { group: "".into(), version: "v1".into(), plural: "configmaps".into(), kind: "ConfigMap".into(), namespaced: true };
    let api = DynamicApi::new(ts.client.clone(), cm, Some("default"));
    for i in 0..5 {
        api.create(&json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": format!("cm-{i}")}})).await.unwrap();
    }
    let mut names = vec![];
    let mut lp = ListParams::default().limit(2);
    loop {
        let page = api.list(&lp).await.unwrap();
        names.extend(page["items"].as_array().unwrap().iter().map(|o| o["metadata"]["name"].as_str().unwrap().to_string()));
        match page["metadata"]["continue"].as_str() {
            Some(c) => lp.continue_token = Some(c.into()),
            None => break,
        }
    }
    assert_eq!(names, ["cm-0", "cm-1", "cm-2", "cm-3", "cm-4"]);
}
