//! Scheduler end-to-end tests with hollow nodes (Go: `test/integration/scheduler`).

use std::collections::BTreeMap;
use std::time::Duration;

use k8s_api::apps::v1::Deployment;
use k8s_api::batch::v1::Job;
use k8s_api::core::v1::{Event, Node, Pod};
use k8s_api::scheduling::v1::PriorityClass;
use k8s_client_go::{Api, ListParams, PatchType};
use kube_integration_tests::{eventually, start_controllers, start_hollow_nodes, start_scheduler, start_test_server};
use serde_json::{json, Value};

const T: Duration = Duration::from_secs(20);

fn from<T: serde::de::DeserializeOwned>(v: Value) -> T {
    serde_json::from_value(v).unwrap()
}

fn pod(name: &str, cpu: &str, extra: Value) -> Pod {
    let mut v = json!({"metadata": {"name": name, "labels": {"app": name}},
        "spec": {"containers": [{"name": "c", "image": "nginx", "resources": {"requests": {"cpu": cpu}}}]}});
    for (k, val) in extra.as_object().cloned().unwrap_or_default() {
        v["spec"][k] = val;
    }
    from(v)
}

fn node_of(p: &Pod) -> Option<String> {
    p.spec.as_ref()?.node_name.clone()
}

async fn wait_scheduled(pods: &Api<Pod>, name: &str) -> String {
    eventually(&format!("{name} scheduled"), T, || async { node_of(&pods.get(name).await.ok()?) }).await
}

async fn wait_unschedulable(pods: &Api<Pod>, name: &str) -> String {
    eventually(&format!("{name} unschedulable"), T, || async {
        let p = pods.get(name).await.ok()?;
        let c = p.status?.conditions?.into_iter().find(|c| c.type_ == "PodScheduled" && c.status == "False")?;
        c.message
    })
    .await
}

#[tokio::test]
async fn deployment_pods_are_scheduled_spread_and_ready() {
    let ts = start_test_server().await;
    let nodes = start_hollow_nodes(&ts.client, "node", 3, "4", &[]).await;
    start_scheduler(&ts.client);
    start_controllers(&ts.client, &["deployment", "replicaset"]);
    let deploys: Api<Deployment> = Api::namespaced(ts.client.clone(), "default");
    deploys
        .create(&from(json!({"metadata": {"name": "web"}, "spec": {"replicas": 6, "selector": {"matchLabels": {"app": "web"}},
            "template": {"metadata": {"labels": {"app": "web"}}, "spec": {"containers": [{"name": "c", "image": "nginx",
                "resources": {"requests": {"cpu": "100m"}}}]}}}})))
        .await
        .unwrap();
    eventually("6 available", T, || async { (deploys.get("web").await.ok()?.status?.available_replicas == Some(6)).then_some(()) }).await;

    let pods: Api<Pod> = Api::namespaced(ts.client.clone(), "default");
    let list = pods.list(&ListParams::default().labels("app=web")).await.unwrap().items;
    let mut per_node: BTreeMap<String, usize> = BTreeMap::new();
    for p in &list {
        *per_node.entry(node_of(p).unwrap()).or_default() += 1;
        assert!(p.status.as_ref().unwrap().pod_ip.as_ref().unwrap().starts_with("10.244."));
    }
    assert_eq!(per_node.len(), 3, "replicas spread over all nodes: {per_node:?}");
    assert!(per_node.values().all(|n| *n == 2), "{per_node:?}");
    assert!(nodes.iter().all(|n| per_node.contains_key(n)));
    // A Scheduled event was recorded.
    let events: Api<Event> = Api::namespaced(ts.client.clone(), "default");
    assert!(events.list(&ListParams::default()).await.unwrap().items.iter().any(|e| e.reason.as_deref() == Some("Scheduled")));
}

#[tokio::test]
async fn insufficient_resources_then_new_node() {
    let ts = start_test_server().await;
    start_hollow_nodes(&ts.client, "small", 2, "1", &[]).await;
    start_scheduler(&ts.client);
    let pods: Api<Pod> = Api::namespaced(ts.client.clone(), "default");
    pods.create(&pod("big", "3", json!({}))).await.unwrap();
    let msg = wait_unschedulable(&pods, "big").await;
    assert!(msg.starts_with("0/2 nodes are available: 2 Insufficient cpu."), "{msg}");
    // Adding a node that fits wakes the pod up.
    let big = start_hollow_nodes(&ts.client, "large", 1, "8", &[]).await;
    assert_eq!(wait_scheduled(&pods, "big").await, big[0]);
    eventually("running", T, || async { (pods.get("big").await.ok()?.status?.phase.as_deref() == Some("Running")).then_some(()) }).await;
}

#[tokio::test]
async fn node_selector_taints_and_unschedulable_nodes() {
    let ts = start_test_server().await;
    let ssd = start_hollow_nodes(&ts.client, "ssd", 1, "4", &[("disk", "ssd")]).await;
    let hdd = start_hollow_nodes(&ts.client, "hdd", 1, "4", &[("disk", "hdd")]).await;
    start_scheduler(&ts.client);
    let pods: Api<Pod> = Api::namespaced(ts.client.clone(), "default");

    pods.create(&pod("fast", "100m", json!({"nodeSelector": {"disk": "ssd"}}))).await.unwrap();
    assert_eq!(wait_scheduled(&pods, "fast").await, ssd[0]);

    // Taint the ssd node: untolerating pods go elsewhere, tolerating ones may stay.
    let nodes: Api<Node> = Api::all(ts.client.clone());
    nodes
        .patch(&ssd[0], PatchType::Merge, &json!({"spec": {"taints": [{"key": "dedicated", "value": "db", "effect": "NoSchedule"}]}}))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    pods.create(&pod("plain", "100m", json!({"nodeAffinity": null}))).await.unwrap();
    assert_eq!(wait_scheduled(&pods, "plain").await, hdd[0]);
    pods.create(&pod(
        "dbpod",
        "100m",
        json!({"nodeSelector": {"disk": "ssd"},
        "tolerations": [{"key": "dedicated", "operator": "Equal", "value": "db", "effect": "NoSchedule"}]}),
    ))
    .await
    .unwrap();
    assert_eq!(wait_scheduled(&pods, "dbpod").await, ssd[0]);

    // Cordon the hdd node: nothing fits anymore.
    nodes.patch(&hdd[0], PatchType::Merge, &json!({"spec": {"unschedulable": true}})).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    pods.create(&pod("stuck", "100m", json!({}))).await.unwrap();
    let msg = wait_unschedulable(&pods, "stuck").await;
    assert!(msg.contains("node(s) were unschedulable") && msg.contains("untolerated taint {dedicated: db}"), "{msg}");
}

#[tokio::test]
async fn required_anti_affinity_and_affinity() {
    let ts = start_test_server().await;
    start_hollow_nodes(&ts.client, "n", 2, "4", &[]).await;
    start_scheduler(&ts.client);
    let pods: Api<Pod> = Api::namespaced(ts.client.clone(), "default");
    let anti = json!({"podAntiAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": [
        {"labelSelector": {"matchLabels": {"role": "db"}}, "topologyKey": "kubernetes.io/hostname"}]}});
    let mut nodes_used = vec![];
    for i in 0..3 {
        let mut p = pod(&format!("db-{i}"), "100m", json!({"affinity": anti}));
        p.metadata.labels.get_or_insert_with(BTreeMap::new).insert("role".into(), "db".into());
        pods.create(&p).await.unwrap();
        if i < 2 {
            nodes_used.push(wait_scheduled(&pods, &format!("db-{i}")).await);
        }
    }
    assert_ne!(nodes_used[0], nodes_used[1]);
    let msg = wait_unschedulable(&pods, "db-2").await;
    assert!(msg.contains("anti-affinity"), "{msg}");

    // Affinity: co-locate with db-0.
    let mut cache = pod(
        "cache",
        "100m",
        json!({"affinity": {"podAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": [
        {"labelSelector": {"matchLabels": {"app": "db-0"}}, "topologyKey": "kubernetes.io/hostname"}]}}}),
    );
    cache.metadata.name = Some("cache".into());
    pods.create(&cache).await.unwrap();
    assert_eq!(wait_scheduled(&pods, "cache").await, nodes_used[0]);
}

#[tokio::test]
async fn preemption_evicts_lower_priority_pods() {
    let ts = start_test_server().await;
    let n = start_hollow_nodes(&ts.client, "tiny", 1, "1", &[]).await;
    start_scheduler(&ts.client);
    let pcs: Api<PriorityClass> = Api::all(ts.client.clone());
    pcs.create(&from(json!({"metadata": {"name": "high"}, "value": 1000}))).await.unwrap();
    pcs.create(&from(json!({"metadata": {"name": "low"}, "value": 10}))).await.unwrap();
    let pods: Api<Pod> = Api::namespaced(ts.client.clone(), "default");

    pods.create(&pod("victim", "800m", json!({"priorityClassName": "low"}))).await.unwrap();
    assert_eq!(wait_scheduled(&pods, "victim").await, n[0]);
    let created = pods.create(&pod("vip", "800m", json!({"priorityClassName": "high"}))).await.unwrap();
    assert_eq!(created.spec.unwrap().priority, Some(1000));
    // The victim is deleted (hollow node completes the graceful deletion), the VIP takes its place.
    eventually("victim gone", T, || async { pods.get_opt("victim").await.ok()?.is_none().then_some(()) }).await;
    assert_eq!(wait_scheduled(&pods, "vip").await, n[0]);
    // Unknown priority class is rejected by admission.
    let err = pods.create(&pod("nope", "1m", json!({"priorityClassName": "missing"}))).await.unwrap_err();
    assert!(err.to_string().contains("no PriorityClass with name missing"), "{err}");
}

#[tokio::test]
async fn scheduling_gates_and_other_schedulers() {
    let ts = start_test_server().await;
    start_hollow_nodes(&ts.client, "g", 1, "4", &[]).await;
    start_scheduler(&ts.client);
    let pods: Api<Pod> = Api::namespaced(ts.client.clone(), "default");
    pods.create(&pod("gated", "100m", json!({"schedulingGates": [{"name": "example.com/wait"}]}))).await.unwrap();
    pods.create(&pod("custom", "100m", json!({"schedulerName": "my-scheduler"}))).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(node_of(&pods.get("gated").await.unwrap()).is_none());
    assert!(node_of(&pods.get("custom").await.unwrap()).is_none());
    // Removing the gate releases the pod.
    pods.patch("gated", PatchType::Merge, &json!({"spec": {"schedulingGates": null}})).await.unwrap();
    wait_scheduled(&pods, "gated").await;
}

#[tokio::test]
async fn job_runs_to_completion_on_hollow_nodes() {
    let ts = start_test_server().await;
    start_hollow_nodes(&ts.client, "j", 2, "4", &[]).await;
    start_scheduler(&ts.client);
    start_controllers(&ts.client, &["job"]);
    let jobs: Api<Job> = Api::namespaced(ts.client.clone(), "default");
    jobs.create(&from(json!({"metadata": {"name": "pi"}, "spec": {"completions": 3, "parallelism": 2,
        "template": {"spec": {"restartPolicy": "Never", "containers": [{"name": "c", "image": "perl"}]}}}})))
        .await
        .unwrap();
    let j = eventually("job complete", T, || async {
        let j = jobs.get("pi").await.ok()?;
        j.status.as_ref()?.conditions.as_ref()?.iter().any(|c| c.type_ == "Complete" && c.status == "True").then_some(j)
    })
    .await;
    assert_eq!(j.status.unwrap().succeeded, Some(3));
}
