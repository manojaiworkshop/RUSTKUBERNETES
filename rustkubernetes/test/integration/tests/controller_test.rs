//! Controllers running against a real in-process API server
//! (Go: `test/integration/{replicaset,deployment,garbagecollector,namespace,...}`).

use std::time::Duration;

use k8s_api::apps::v1::{DaemonSet, Deployment, ReplicaSet, StatefulSet};
use k8s_api::batch::v1::Job;
use k8s_api::core::v1::{ConfigMap, Endpoints, Namespace, Node, Pod, Service, ServiceAccount};
use k8s_api::discovery::v1::EndpointSlice;
use k8s_apimachinery::meta::ObjectMeta;
use k8s_client_go::tools::leaderelection::{LeaderElectionConfig, LeaderElector};
use k8s_client_go::{Api, DeleteParams, ListParams, PatchType};
use kube_integration_tests::{eventually, simulate_kubelet, start_controllers, start_test_server};
use serde_json::{json, Value};

const T: Duration = Duration::from_secs(20);

fn from<T: serde::de::DeserializeOwned>(v: Value) -> T {
    serde_json::from_value(v).unwrap()
}

fn template(labels: Value, image: &str) -> Value {
    json!({"metadata": {"labels": labels}, "spec": {"containers": [{"name": "app", "image": image}]}})
}

fn deployment(name: &str, replicas: i32, image: &str) -> Deployment {
    from(json!({"metadata": {"name": name}, "spec": {"replicas": replicas,
        "selector": {"matchLabels": {"app": name}}, "template": template(json!({"app": name}), image)}}))
}

async fn pods_owned_by(api: &Api<Pod>, uid: &str) -> Vec<Pod> {
    api.list(&ListParams::default())
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|p| p.metadata.controller_ref().is_some_and(|r| r.uid == uid))
        .collect()
}

#[tokio::test]
async fn replicaset_maintains_replicas() {
    let ts = start_test_server().await;
    start_controllers(&ts.client, &["replicaset-controller"]);
    let rss: Api<ReplicaSet> = Api::namespaced(ts.client.clone(), "default");
    let pods: Api<Pod> = Api::namespaced(ts.client.clone(), "default");
    let rs: ReplicaSet = from(json!({"metadata": {"name": "web"}, "spec": {"replicas": 3,
        "selector": {"matchLabels": {"app": "web"}}, "template": template(json!({"app": "web"}), "nginx")}}));
    let rs = rss.create(&rs).await.unwrap();
    let uid = rs.metadata.uid.clone().unwrap();

    eventually("3 pods", T, || async { (pods_owned_by(&pods, &uid).await.len() == 3).then_some(()) }).await;
    let first = pods_owned_by(&pods, &uid).await[0].metadata.name().to_string();
    assert!(first.starts_with("web-"));
    // A deleted pod is replaced.
    pods.delete(&first).await.unwrap();
    eventually("pod replaced", T, || async {
        let p = pods_owned_by(&pods, &uid).await;
        (p.len() == 3 && p.iter().all(|x| x.metadata.name() != first)).then_some(())
    })
    .await;
    // Scale down.
    rss.patch("web", PatchType::Merge, &json!({"spec": {"replicas": 1}})).await.unwrap();
    eventually("1 pod", T, || async { (pods_owned_by(&pods, &uid).await.len() == 1).then_some(()) }).await;
    eventually("status.replicas=1", T, || async { (rss.get("web").await.ok()?.status?.replicas == 1).then_some(()) }).await;
    // An orphan pod matching the selector is adopted (and then deleted as surplus).
    let orphan: Pod =
        from(json!({"metadata": {"name": "stray", "labels": {"app": "web"}}, "spec": {"containers": [{"name": "c", "image": "x"}]}}));
    pods.create(&orphan).await.unwrap();
    eventually("orphan adopted or removed", T, || async {
        let p = pods_owned_by(&pods, &uid).await;
        (p.len() == 1).then_some(())
    })
    .await;
}

#[tokio::test]
async fn deployment_rolling_update_and_history() {
    let ts = start_test_server().await;
    start_controllers(&ts.client, &["deployment-controller", "replicaset-controller"]);
    simulate_kubelet(&ts.client);
    let deploys: Api<Deployment> = Api::namespaced(ts.client.clone(), "default");
    let rss: Api<ReplicaSet> = Api::namespaced(ts.client.clone(), "default");
    deploys.create(&deployment("web", 3, "nginx:1.27")).await.unwrap();

    let d = eventually("deployment available", T, || async {
        let d = deploys.get("web").await.ok()?;
        let st = d.status.clone()?;
        (st.available_replicas == Some(3) && st.updated_replicas == Some(3)).then_some(d)
    })
    .await;
    let conds = d.status.unwrap().conditions.unwrap();
    assert!(conds.iter().any(|c| c.type_ == "Available" && c.status == "True"));
    assert!(conds.iter().any(|c| c.type_ == "Progressing" && c.reason.as_deref() == Some("NewReplicaSetAvailable")));
    assert_eq!(d.metadata.annotations.unwrap()["deployment.kubernetes.io/revision"], "1");

    // Rolling update to a new image.
    deploys
        .patch(
            "web",
            PatchType::Strategic,
            &json!({"spec": {"template": {"spec": {"containers": [{"name": "app", "image": "nginx:1.28"}]}}}}),
        )
        .await
        .unwrap();
    eventually("rollout complete", T, || async {
        let list = rss.list(&ListParams::default()).await.ok()?.items;
        let image = |rs: &ReplicaSet| {
            rs.spec.as_ref().and_then(|s| s.template.as_ref()).and_then(|t| t.spec.as_ref()).and_then(|s| s.containers[0].image.clone())
        };
        let new = list.iter().find(|rs| image(rs).as_deref() == Some("nginx:1.28"))?;
        let old = list.iter().find(|rs| rs.metadata.uid != new.metadata.uid)?;
        let d = deploys.get("web").await.ok()?;
        let st = d.status?;
        (new.spec.as_ref()?.replicas == Some(3)
            && old.spec.as_ref()?.replicas == Some(0)
            && st.updated_replicas == Some(3)
            && st.available_replicas == Some(3)
            && st.replicas == Some(3)
            && new.metadata.annotations.as_ref()?["deployment.kubernetes.io/revision"] == "2")
            .then_some(())
    })
    .await;
    let d = deploys.get("web").await.unwrap();
    assert_eq!(d.metadata.annotations.unwrap()["deployment.kubernetes.io/revision"], "2");

    // Scaling is applied to the current ReplicaSet.
    deploys.patch("web", PatchType::Merge, &json!({"spec": {"replicas": 5}})).await.unwrap();
    eventually("scaled to 5", T, || async { (deploys.get("web").await.ok()?.status?.available_replicas == Some(5)).then_some(()) }).await;
}

#[tokio::test]
async fn deployment_recreate() {
    let ts = start_test_server().await;
    start_controllers(&ts.client, &["deployment", "replicaset"]);
    simulate_kubelet(&ts.client);
    let deploys: Api<Deployment> = Api::namespaced(ts.client.clone(), "default");
    let mut d = deployment("db", 2, "postgres:16");
    d.spec.as_mut().unwrap().strategy = Some(from(json!({"type": "Recreate"})));
    deploys.create(&d).await.unwrap();
    eventually("available", T, || async { (deploys.get("db").await.ok()?.status?.available_replicas == Some(2)).then_some(()) }).await;
    deploys
        .patch(
            "db",
            PatchType::Strategic,
            &json!({"spec": {"template": {"spec": {"containers": [{"name": "app", "image": "postgres:17"}]}}}}),
        )
        .await
        .unwrap();
    eventually("recreated", T, || async {
        let st = deploys.get("db").await.ok()?.status?;
        (st.updated_replicas == Some(2) && st.available_replicas == Some(2) && st.replicas == Some(2)).then_some(())
    })
    .await;
}

#[tokio::test]
async fn garbage_collector_propagation_policies() {
    let ts = start_test_server().await;
    start_controllers(&ts.client, &["deployment", "replicaset", "garbagecollector"]);
    let deploys: Api<Deployment> = Api::namespaced(ts.client.clone(), "default");
    let rss: Api<ReplicaSet> = Api::namespaced(ts.client.clone(), "default");
    let pods: Api<Pod> = Api::namespaced(ts.client.clone(), "default");

    // Background: owner goes first, dependents follow.
    let bg = deploys.create(&deployment("bg", 2, "nginx")).await.unwrap();
    let bg_uid = bg.metadata.uid.clone().unwrap();
    eventually("bg pods", T, || async { (pods.list(&ListParams::default().labels("app=bg")).await.ok()?.items.len() == 2).then_some(()) })
        .await;
    deploys.delete_with("bg", &DeleteParams::background()).await.unwrap();
    eventually("bg cascade", T, || async {
        let rs_left =
            rss.list(&ListParams::default()).await.ok()?.items.iter().any(|r| r.metadata.controller_ref().is_some_and(|o| o.uid == bg_uid));
        let pods_left = pods.list(&ListParams::default().labels("app=bg")).await.ok()?.items.len();
        (!rs_left && pods_left == 0).then_some(())
    })
    .await;

    // Foreground: owner stays (with the finalizer) until dependents are gone.
    deploys.create(&deployment("fg", 1, "nginx")).await.unwrap();
    eventually("fg pods", T, || async { (pods.list(&ListParams::default().labels("app=fg")).await.ok()?.items.len() == 1).then_some(()) })
        .await;
    deploys.delete_with("fg", &DeleteParams::foreground()).await.unwrap();
    let held = deploys.get("fg").await.unwrap();
    assert!(held.metadata.finalizers.unwrap().contains(&"foregroundDeletion".to_string()));
    eventually("fg deleted", T, || async { deploys.get_opt("fg").await.ok()?.is_none().then_some(()) }).await;
    assert!(pods.list(&ListParams::default().labels("app=fg")).await.unwrap().items.is_empty());

    // Orphan: dependents survive without the ownerReference.
    let rs: ReplicaSet = from(json!({"metadata": {"name": "orph"}, "spec": {"replicas": 2,
        "selector": {"matchLabels": {"app": "orph"}}, "template": template(json!({"app": "orph"}), "nginx")}}));
    rss.create(&rs).await.unwrap();
    eventually("orph pods", T, || async {
        (pods.list(&ListParams::default().labels("app=orph")).await.ok()?.items.len() == 2).then_some(())
    })
    .await;
    rss.delete_with("orph", &DeleteParams::orphan()).await.unwrap();
    eventually("orphaned", T, || async {
        let gone = rss.get_opt("orph").await.ok()?.is_none();
        let left = pods.list(&ListParams::default().labels("app=orph")).await.ok()?.items;
        (gone && left.len() == 2 && left.iter().all(|p| p.metadata.owner_references.as_ref().is_none_or(|r| r.is_empty()))).then_some(())
    })
    .await;
}

#[tokio::test]
async fn namespace_deletion_and_default_serviceaccount() {
    let ts = start_test_server().await;
    start_controllers(&ts.client, &["namespace", "serviceaccount", "deployment", "replicaset", "garbagecollector"]);
    let nss: Api<Namespace> = Api::all(ts.client.clone());
    nss.create(&Namespace::new(ObjectMeta::named("team"))).await.unwrap();
    let sas: Api<ServiceAccount> = Api::namespaced(ts.client.clone(), "team");
    eventually("default SA", T, || async { sas.get_opt("default").await.ok()?.map(|_| ()) }).await;

    let cms: Api<ConfigMap> = Api::namespaced(ts.client.clone(), "team");
    cms.create(&ConfigMap::new(ObjectMeta::named("cfg"))).await.unwrap();
    let deploys: Api<Deployment> = Api::namespaced(ts.client.clone(), "team");
    deploys.create(&deployment("api", 2, "nginx")).await.unwrap();
    let pods: Api<Pod> = Api::namespaced(ts.client.clone(), "team");
    eventually("team pods", T, || async { (pods.list(&ListParams::default()).await.ok()?.items.len() == 2).then_some(()) }).await;

    nss.delete("team").await.unwrap();
    eventually("namespace gone", T, || async { nss.get_opt("team").await.ok()?.is_none().then_some(()) }).await;
    assert!(cms.get_opt("cfg").await.unwrap().is_none());
    assert!(pods.list(&ListParams::default()).await.unwrap().items.is_empty());
}

#[tokio::test]
async fn endpoints_and_endpointslices_follow_ready_pods() {
    let ts = start_test_server().await;
    start_controllers(&ts.client, &["deployment", "replicaset", "endpoints-controller", "endpointslice-controller", "garbagecollector"]);
    simulate_kubelet(&ts.client);
    let deploys: Api<Deployment> = Api::namespaced(ts.client.clone(), "default");
    let mut d = deployment("web", 3, "nginx");
    d.spec.as_mut().unwrap().template.spec.as_mut().unwrap().containers[0].ports =
        Some(vec![from(json!({"name": "http", "containerPort": 8080}))]);
    deploys.create(&d).await.unwrap();
    let svcs: Api<Service> = Api::namespaced(ts.client.clone(), "default");
    svcs.create(&from(json!({"metadata": {"name": "web"}, "spec": {"selector": {"app": "web"}, "ports": [{"name": "http", "port": 80, "targetPort": "http"}]}})))
        .await
        .unwrap();

    let eps: Api<Endpoints> = Api::namespaced(ts.client.clone(), "default");
    let ep = eventually("3 endpoints", T, || async {
        let ep = eps.get_opt("web").await.ok()??;
        let n = ep.subsets.as_ref()?.iter().map(|s| s.addresses.as_ref().map(Vec::len).unwrap_or(0)).sum::<usize>();
        (n == 3).then_some(ep)
    })
    .await;
    assert_eq!(ep.subsets.unwrap()[0].ports.as_ref().unwrap()[0].port, 8080);

    let slices: Api<EndpointSlice> = Api::namespaced(ts.client.clone(), "default");
    let slice = eventually("slice with 3 endpoints", T, || async {
        let list = slices.list(&ListParams::default().labels("kubernetes.io/service-name=web")).await.ok()?.items;
        let s = list.into_iter().next()?;
        (s.endpoints.len() == 3 && s.endpoints.iter().all(|e| e.conditions.as_ref().and_then(|c| c.ready) == Some(true))).then_some(s)
    })
    .await;
    assert_eq!(slice.ports.unwrap()[0].port, Some(8080));
    assert_eq!(slice.address_type, "IPv4");

    // Scale down → endpoints shrink; deleting the service removes its slices (GC).
    deploys.patch("web", PatchType::Merge, &json!({"spec": {"replicas": 1}})).await.unwrap();
    eventually("1 endpoint", T, || async {
        let ep = eps.get_opt("web").await.ok()??;
        (ep.subsets?.iter().map(|s| s.addresses.as_ref().map(Vec::len).unwrap_or(0)).sum::<usize>() == 1).then_some(())
    })
    .await;
    svcs.delete("web").await.unwrap();
    eventually("slices gone", T, || async {
        slices.list(&ListParams::default().labels("kubernetes.io/service-name=web")).await.ok()?.items.is_empty().then_some(())
    })
    .await;
    eventually("endpoints gone", T, || async { eps.get_opt("web").await.ok()?.is_none().then_some(()) }).await;
}

fn job(name: &str, completions: i32, backoff: i32) -> Job {
    from(json!({"metadata": {"name": name}, "spec": {"completions": completions, "parallelism": 1, "backoffLimit": backoff,
        "template": {"metadata": {"labels": {"test/manual": "true"}},
                     "spec": {"restartPolicy": "Never", "containers": [{"name": "c", "image": "busybox"}]}}}}))
}

/// Marks every active pod of the job with the given phase (what the kubelet would report).
async fn finish_pods(pods: &Api<Pod>, job: &str, phase: &str) -> usize {
    let mut n = 0;
    for mut p in pods.list(&ListParams::default().labels(&format!("batch.kubernetes.io/job-name={job}"))).await.unwrap().items {
        if p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Pending") {
            p.status.get_or_insert_with(Default::default).phase = Some(phase.into());
            pods.replace_status(&p).await.unwrap();
            n += 1;
        }
    }
    n
}

#[tokio::test]
async fn job_completes_and_fails() {
    let ts = start_test_server().await;
    start_controllers(&ts.client, &["job"]);
    let jobs: Api<Job> = Api::namespaced(ts.client.clone(), "default");
    let pods: Api<Pod> = Api::namespaced(ts.client.clone(), "default");

    jobs.create(&job("ok", 2, 6)).await.unwrap();
    for _ in 0..2 {
        eventually("job pod", T, || async { (finish_pods(&pods, "ok", "Succeeded").await == 1).then_some(()) }).await;
    }
    let j = eventually("complete", T, || async {
        let j = jobs.get("ok").await.ok()?;
        j.status.as_ref()?.conditions.as_ref()?.iter().any(|c| c.type_ == "Complete" && c.status == "True").then_some(j)
    })
    .await;
    assert_eq!(j.status.unwrap().succeeded, Some(2));

    jobs.create(&job("bad", 1, 1)).await.unwrap();
    for _ in 0..2 {
        eventually("failing pod", T, || async { (finish_pods(&pods, "bad", "Failed").await == 1).then_some(()) }).await;
    }
    let j = eventually("failed", T, || async {
        let j = jobs.get("bad").await.ok()?;
        j.status.as_ref()?.conditions.as_ref()?.iter().find(|c| c.type_ == "Failed" && c.status == "True").cloned()
    })
    .await;
    assert_eq!(j.reason.as_deref(), Some("BackoffLimitExceeded"));
}

#[tokio::test]
async fn statefulset_creates_ordered_pods() {
    let ts = start_test_server().await;
    start_controllers(&ts.client, &["statefulset"]);
    simulate_kubelet(&ts.client);
    let sts: Api<StatefulSet> = Api::namespaced(ts.client.clone(), "default");
    sts.create(&from(json!({"metadata": {"name": "db"}, "spec": {"replicas": 3, "serviceName": "db",
        "selector": {"matchLabels": {"app": "db"}}, "template": template(json!({"app": "db"}), "postgres")}})))
        .await
        .unwrap();
    eventually("3 ready", T, || async { (sts.get("db").await.ok()?.status?.ready_replicas == Some(3)).then_some(()) }).await;
    let pods: Api<Pod> = Api::namespaced(ts.client.clone(), "default");
    let mut names: Vec<String> =
        pods.list(&ListParams::default().labels("app=db")).await.unwrap().items.iter().map(|p| p.metadata.name().to_string()).collect();
    names.sort();
    assert_eq!(names, ["db-0", "db-1", "db-2"]);
    let p0 = pods.get("db-0").await.unwrap();
    assert_eq!(p0.spec.as_ref().unwrap().hostname.as_deref(), Some("db-0"));
    assert_eq!(p0.spec.unwrap().subdomain.as_deref(), Some("db"));
    // Scale down removes the highest ordinal.
    sts.patch("db", PatchType::Merge, &json!({"spec": {"replicas": 2}})).await.unwrap();
    eventually("db-2 removed", T, || async { pods.get_opt("db-2").await.ok()?.is_none().then_some(()) }).await;
}

fn ready_node(name: &str, heartbeat_age_secs: i64) -> Node {
    let t = (chrono::Utc::now() - chrono::Duration::seconds(heartbeat_age_secs)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    from(json!({"metadata": {"name": name, "labels": {"kubernetes.io/hostname": name}},
        "status": {"conditions": [{"type": "Ready", "status": "True", "lastHeartbeatTime": t, "lastTransitionTime": t}]}}))
}

#[tokio::test]
async fn daemonset_runs_one_pod_per_node() {
    let ts = start_test_server().await;
    let nodes: Api<Node> = Api::all(ts.client.clone());
    for n in ["n1", "n2"] {
        let node = nodes.create(&ready_node(n, 0)).await.unwrap();
        nodes.replace_status(&Node { status: ready_node(n, 0).status, ..node }).await.unwrap();
    }
    start_controllers(&ts.client, &["daemonset"]);
    let dss: Api<DaemonSet> = Api::namespaced(ts.client.clone(), "kube-system");
    dss.create(&from(json!({"metadata": {"name": "agent"}, "spec": {
        "selector": {"matchLabels": {"app": "agent"}}, "template": template(json!({"app": "agent"}), "fluent-bit")}})))
        .await
        .unwrap();
    let pods: Api<Pod> = Api::namespaced(ts.client.clone(), "kube-system");
    let list = eventually("2 daemon pods", T, || async {
        let l = pods.list(&ListParams::default().labels("app=agent")).await.ok()?.items;
        (l.len() == 2).then_some(l)
    })
    .await;
    let mut targets: Vec<String> = list
        .iter()
        .map(|p| {
            p.spec.as_ref().unwrap().affinity.as_ref().unwrap()["nodeAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"]
                ["nodeSelectorTerms"][0]["matchFields"][0]["values"][0]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    targets.sort();
    assert_eq!(targets, ["n1", "n2"]);
    eventually("desired=2", T, || async { (dss.get("agent").await.ok()?.status?.desired_number_scheduled == 2).then_some(()) }).await;
}

#[tokio::test]
async fn node_lifecycle_taints_and_evicts() {
    let ts = start_test_server().await;
    let nodes: Api<Node> = Api::all(ts.client.clone());
    let node = nodes.create(&ready_node("stale", 600)).await.unwrap();
    nodes.replace_status(&Node { status: ready_node("stale", 600).status, ..node }).await.unwrap();
    // A pod bound to the node that tolerates `unreachable` for 0 seconds.
    let pods: Api<Pod> = Api::namespaced(ts.client.clone(), "default");
    pods.create(&from(json!({"metadata": {"name": "victim"}, "spec": {"containers": [{"name": "c", "image": "x"}],
        "tolerations": [{"key": "node.kubernetes.io/unreachable", "operator": "Exists", "effect": "NoExecute", "tolerationSeconds": 0}]}})))
        .await
        .unwrap();
    pods.dynamic().create_subresource("victim", "binding", &json!({"target": {"kind": "Node", "name": "stale"}})).await.unwrap();

    start_controllers(&ts.client, &["nodelifecycle", "taint-eviction"]);
    let n = eventually("node unknown + tainted", T, || async {
        let n = nodes.get("stale").await.ok()?;
        let ready = n.status.as_ref()?.conditions.as_ref()?.iter().find(|c| c.type_ == "Ready")?.status.clone();
        let tainted =
            n.spec.as_ref()?.taints.as_ref()?.iter().any(|t| t.key == "node.kubernetes.io/unreachable" && t.effect == "NoExecute");
        (ready == "Unknown" && tainted).then_some(n)
    })
    .await;
    assert!(n.spec.unwrap().taints.unwrap().iter().any(|t| t.effect == "NoSchedule"));
    eventually("pod evicted", T, || async {
        let p = pods.get("victim").await.ok()?;
        p.metadata.deletion_timestamp.map(|_| ())
    })
    .await;
}

#[tokio::test]
async fn leader_election_is_exclusive() {
    let ts = start_test_server().await;
    let a = LeaderElector::new(ts.client.clone(), LeaderElectionConfig::new("kube-system", "test-lock", "a"));
    let b = LeaderElector::new(ts.client.clone(), LeaderElectionConfig::new("kube-system", "test-lock", "b"));
    assert!(a.try_acquire_or_renew().await.unwrap());
    assert!(!b.try_acquire_or_renew().await.unwrap());
    assert!(a.try_acquire_or_renew().await.unwrap()); // renew
    a.release().await;
    assert!(b.try_acquire_or_renew().await.unwrap());
}
