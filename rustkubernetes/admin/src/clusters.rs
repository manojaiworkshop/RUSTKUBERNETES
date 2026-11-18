//! Clusters: provisioning on machines, importing, lifecycle, live state and access.
//!
//! **Provisioning** a managed cluster (master machine + worker machines):
//! 1. `kubeadm init` on the rustkubeadmin host creates the cluster PKI and
//!    kubeconfigs in `<data>/clusters/<name>`; `kubeadm node-credentials` issues
//!    each worker's kubelet certificate. The CA key never leaves rustkubeadmin.
//! 2. Each machine gets a payload (certificates, kubeconfigs and one script per
//!    service) unpacked into `<prefix>/clusters/<name>`, then
//!    `rustkube-node start <name>` runs the services under systemd (or a fallback).
//! 3. rustkubeadmin waits for the API server and for every node to be Ready, then
//!    deploys the CoreDNS add-on.
//!
//! **Live state**: for every running cluster, informers (pods, nodes,
//! deployments, services, namespaces, events) feed a snapshot published on the
//! `cluster:<name>` topic about once a second when something changed.
//!
//! **Access**: a console user's grants become RoleBindings / ClusterRoleBindings
//! for the Kubernetes user `rustkubeadmin:<username>`; console requests are sent
//! with `Impersonate-User`, so Kubernetes RBAC decides what each user may do.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use k8s_api::apps::v1::Deployment;
use k8s_api::core::v1::{Event as KEvent, Namespace, Node, Pod, Service};
use k8s_client_go::tools::cache::{Informer, Store};
use k8s_client_go::tools::clientcmd;
use k8s_client_go::{Api, Client, ListParams};
use serde_json::{json, Value};

use crate::auth::{KUBE_GROUP, KUBE_USER_PREFIX};
use crate::bundle::KUBE_PROXY_IMAGE;
use crate::jobs::JobHandle;
use crate::state::{dns_label, Cluster, Machine};
use crate::transport::sh_quote;
use crate::App;

pub const COREDNS_YAML: &str = include_str!("../../cluster/addons/dns/coredns/coredns.yaml");
pub const CLUSTER_DNS: &str = "10.96.0.10";
const MANAGED_LABEL: &str = "rustkubeadmin.io/managed";

/// A running cluster's connection and live watches.
pub struct Runtime {
    pub client: Client,
    stop: Vec<Box<dyn Fn() + Send + Sync>>,
    pods: Store<Pod>,
    nodes: Store<Node>,
    deployments: Store<Deployment>,
    services: Store<Service>,
    namespaces: Store<Namespace>,
    events: Store<KEvent>,
    dirty: Arc<AtomicBool>,
    pub reachable: AtomicBool,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        for s in &self.stop {
            s();
        }
    }
}

/// Admin (cluster-admin) client for a cluster.
pub fn admin_client(app: &App, c: &Cluster) -> Result<Client> {
    let dir = app.store.cluster_dir(&c.name);
    let path = if c.kind == "imported" { dir.join("kubeconfig") } else { dir.join("super-admin.conf") };
    let mut cfg = clientcmd::load_config(&path, None).map_err(|e| anyhow!("{e}"))?;
    cfg.timeout = Duration::from_secs(20);
    cfg.user_agent = format!("rustkubeadmin/{}", env!("CARGO_PKG_VERSION"));
    Ok(Client::new(cfg)?)
}

fn watch<K>(client: &Client, dirty: &Arc<AtomicBool>, stop: &mut Vec<Box<dyn Fn() + Send + Sync>>) -> Store<K>
where
    K: k8s_client_go::tools::cache::CacheObject + k8s_api::Resource + serde::de::DeserializeOwned + Send + Sync + 'static,
{
    let inf = Informer::new(Api::<K>::all(client.clone()), ListParams::default());
    let mut rx = inf.subscribe();
    let d = dirty.clone();
    let t = tokio::spawn(async move {
        // Any event (or a lag, which means many events) marks the snapshot stale; a closed channel ends the task.
        while !matches!(rx.recv().await, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
            d.store(true, Ordering::Relaxed);
        }
    });
    inf.start();
    let store = inf.store();
    let inf2 = inf.clone();
    stop.push(Box::new(move || {
        inf2.stop();
        t.abort();
    }));
    store
}

/// Starts (or restarts) live watches for a running cluster.
pub fn start_runtime(app: &Arc<App>, name: &str) {
    let Some(c) = app.store.cluster(name) else { return };
    let client = match admin_client(app, &c) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(cluster = name, "cannot connect: {e:#}");
            return;
        }
    };
    let dirty = Arc::new(AtomicBool::new(true));
    let mut stop: Vec<Box<dyn Fn() + Send + Sync>> = vec![];
    let rt = Arc::new(Runtime {
        pods: watch::<Pod>(&client, &dirty, &mut stop),
        nodes: watch::<Node>(&client, &dirty, &mut stop),
        deployments: watch::<Deployment>(&client, &dirty, &mut stop),
        services: watch::<Service>(&client, &dirty, &mut stop),
        namespaces: watch::<Namespace>(&client, &dirty, &mut stop),
        events: watch::<KEvent>(&client, &dirty, &mut stop),
        client,
        stop,
        dirty: dirty.clone(),
        reachable: AtomicBool::new(false),
    });
    app.runtimes.lock().unwrap().insert(name.to_string(), rt.clone());
    // Publisher: at most once a second, only when something changed (and every 10 s for ages).
    let (app2, name2, weak) = (app.clone(), name.to_string(), Arc::downgrade(&rt));
    tokio::spawn(async move {
        let mut tick = 0u32;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let Some(rt) = weak.upgrade() else { break };
            tick += 1;
            if tick.is_multiple_of(5) {
                let ok = rt.client.get_text("/readyz").await.is_ok();
                if rt.reachable.swap(ok, Ordering::Relaxed) != ok {
                    rt.dirty.store(true, Ordering::Relaxed);
                }
            }
            if rt.dirty.swap(false, Ordering::Relaxed) || tick.is_multiple_of(10) {
                app2.hub.publish(&format!("cluster:{name2}"), snapshot(&rt));
                publish_clusters(&app2);
            }
        }
    });
}

pub fn stop_runtime(app: &App, name: &str) {
    app.runtimes.lock().unwrap().remove(name);
    app.hub.forget(&format!("cluster:{name}"));
}

fn s(v: &Value, ptr: &str) -> String {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or("").to_string()
}

/// `kubectl get pods` STATUS column logic (simplified `printPod`).
fn pod_status(p: &Value) -> (String, String, i64) {
    let mut reason = s(p, "/status/phase");
    let r = s(p, "/status/reason");
    if !r.is_empty() {
        reason = r;
    }
    let statuses = p.pointer("/status/containerStatuses").and_then(Value::as_array).cloned().unwrap_or_default();
    let total = p.pointer("/spec/containers").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
    let mut ready = 0;
    let mut restarts = 0;
    for (i, init) in p.pointer("/status/initContainerStatuses").and_then(Value::as_array).cloned().unwrap_or_default().iter().enumerate() {
        if init.pointer("/state/terminated/exitCode").and_then(Value::as_i64) == Some(0) {
            continue;
        }
        let w = s(init, "/state/waiting/reason");
        if !w.is_empty() && w != "PodInitializing" {
            reason = format!("Init:{w}");
        } else {
            reason = format!("Init:{i}/{}", p.pointer("/spec/initContainers").and_then(Value::as_array).map(Vec::len).unwrap_or(0));
        }
        break;
    }
    if !reason.starts_with("Init:") {
        for c in statuses.iter().rev() {
            restarts += c["restartCount"].as_i64().unwrap_or(0);
            let w = s(c, "/state/waiting/reason");
            let t = s(c, "/state/terminated/reason");
            if !w.is_empty() {
                reason = w;
            } else if !t.is_empty() {
                reason = t;
            } else if c["ready"] == true {
                ready += 1;
            }
        }
    }
    if p.pointer("/metadata/deletionTimestamp").is_some() {
        reason = "Terminating".into();
    }
    (reason, format!("{ready}/{total}"), restarts)
}

fn snapshot(rt: &Runtime) -> Value {
    let to = |o: &dyn erased::Ser| o.value();
    let mut pods: Vec<Value> = rt
        .pods
        .list()
        .iter()
        .map(|p| {
            let v = to(p.as_ref());
            let (status, ready, restarts) = pod_status(&v);
            json!({
                "name": s(&v, "/metadata/name"), "namespace": s(&v, "/metadata/namespace"),
                "status": status, "phase": s(&v, "/status/phase"), "ready": ready, "restarts": restarts,
                "node": s(&v, "/spec/nodeName"), "ip": s(&v, "/status/podIP"),
                "created": s(&v, "/metadata/creationTimestamp"),
                "owner": v.pointer("/metadata/ownerReferences/0").map(|o| format!("{}/{}", s(o, "/kind"), s(o, "/name"))).unwrap_or_default(),
                "containers": v.pointer("/spec/containers").and_then(Value::as_array).map(|cs| cs.iter().map(|c| json!({"name": c["name"], "image": c["image"]})).collect::<Vec<_>>()).unwrap_or_default(),
            })
        })
        .collect();
    pods.sort_by(|a, b| (a["namespace"].as_str(), a["name"].as_str()).cmp(&(b["namespace"].as_str(), b["name"].as_str())));
    let mut nodes: Vec<Value> = rt
        .nodes
        .list()
        .iter()
        .map(|n| {
            let v = to(n.as_ref());
            let ready = v
                .pointer("/status/conditions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|c| c["type"] == "Ready")
                .map(|c| s(c, "/status"))
                .unwrap_or_default();
            let ip = v
                .pointer("/status/addresses")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|a| a["type"] == "InternalIP")
                .map(|a| s(a, "/address"))
                .unwrap_or_default();
            json!({
                "name": s(&v, "/metadata/name"), "ready": ready == "True", "ip": ip,
                "unschedulable": v.pointer("/spec/unschedulable") == Some(&json!(true)),
                "version": s(&v, "/status/nodeInfo/kubeletVersion"), "os": s(&v, "/status/nodeInfo/osImage"),
                "kernel": s(&v, "/status/nodeInfo/kernelVersion"), "runtime": s(&v, "/status/nodeInfo/containerRuntimeVersion"),
                "cpu": s(&v, "/status/capacity/cpu"), "memory": s(&v, "/status/capacity/memory"), "maxPods": s(&v, "/status/capacity/pods"),
                "created": s(&v, "/metadata/creationTimestamp"),
                "taints": v.pointer("/spec/taints").cloned().unwrap_or(json!([])),
            })
        })
        .collect();
    nodes.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let mut deployments: Vec<Value> = rt
        .deployments
        .list()
        .iter()
        .map(|d| {
            let v = to(d.as_ref());
            json!({
                "name": s(&v, "/metadata/name"), "namespace": s(&v, "/metadata/namespace"),
                "replicas": v.pointer("/spec/replicas").and_then(Value::as_i64).unwrap_or(1),
                "ready": v.pointer("/status/readyReplicas").and_then(Value::as_i64).unwrap_or(0),
                "updated": v.pointer("/status/updatedReplicas").and_then(Value::as_i64).unwrap_or(0),
                "available": v.pointer("/status/availableReplicas").and_then(Value::as_i64).unwrap_or(0),
                "images": v.pointer("/spec/template/spec/containers").and_then(Value::as_array).map(|cs| cs.iter().map(|c| s(c, "/image")).collect::<Vec<_>>()).unwrap_or_default(),
                "created": s(&v, "/metadata/creationTimestamp"),
            })
        })
        .collect();
    deployments.sort_by(|a, b| (a["namespace"].as_str(), a["name"].as_str()).cmp(&(b["namespace"].as_str(), b["name"].as_str())));
    let mut services: Vec<Value> = rt
        .services
        .list()
        .iter()
        .map(|x| {
            let v = to(x.as_ref());
            let ports: Vec<String> = v
                .pointer("/spec/ports")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|p| {
                    let np = p["nodePort"].as_i64().map(|n| format!(":{n}")).unwrap_or_default();
                    format!("{}{np}/{}", p["port"], p["protocol"].as_str().unwrap_or("TCP"))
                })
                .collect();
            json!({
                "name": s(&v, "/metadata/name"), "namespace": s(&v, "/metadata/namespace"),
                "type": s(&v, "/spec/type"), "clusterIP": s(&v, "/spec/clusterIP"), "ports": ports.join(", "),
                "selector": v.pointer("/spec/selector").cloned().unwrap_or(json!({})),
                "created": s(&v, "/metadata/creationTimestamp"),
            })
        })
        .collect();
    services.sort_by(|a, b| (a["namespace"].as_str(), a["name"].as_str()).cmp(&(b["namespace"].as_str(), b["name"].as_str())));
    let mut namespaces: Vec<Value> = rt
        .namespaces
        .list()
        .iter()
        .map(|n| {
            let v = to(n.as_ref());
            json!({"name": s(&v, "/metadata/name"), "phase": s(&v, "/status/phase"), "created": s(&v, "/metadata/creationTimestamp")})
        })
        .collect();
    namespaces.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let mut events: Vec<Value> = rt
        .events
        .list()
        .iter()
        .map(|e| {
            let v = to(e.as_ref());
            let time = [s(&v, "/lastTimestamp"), s(&v, "/eventTime"), s(&v, "/metadata/creationTimestamp")]
                .into_iter()
                .find(|t| !t.is_empty())
                .unwrap_or_default();
            json!({
                "namespace": s(&v, "/metadata/namespace"), "type": s(&v, "/type"), "reason": s(&v, "/reason"),
                "object": format!("{}/{}", s(&v, "/involvedObject/kind").to_lowercase(), s(&v, "/involvedObject/name")),
                "message": s(&v, "/message"), "count": v["count"].as_i64().unwrap_or(1), "time": time,
            })
        })
        .collect();
    events.sort_by(|a, b| b["time"].as_str().cmp(&a["time"].as_str()));
    events.truncate(150);
    json!({
        "reachable": rt.reachable.load(Ordering::Relaxed),
        "nodes": nodes, "pods": pods, "deployments": deployments, "services": services,
        "namespaces": namespaces, "events": events,
    })
}

/// Serialising typed objects without naming every type.
mod erased {
    pub trait Ser {
        fn value(&self) -> serde_json::Value;
    }
    impl<T: serde::Serialize> Ser for T {
        fn value(&self) -> serde_json::Value {
            serde_json::to_value(self).unwrap_or_default()
        }
    }
}

/// The `cluster:<name>` snapshot reduced to what `namespaces` may see (`None` = everything).
pub fn filter_snapshot(mut v: Value, namespaces: &Option<Vec<String>>) -> Value {
    let Some(ns) = namespaces else { return v };
    for key in ["pods", "deployments", "services", "events"] {
        if let Some(a) = v.get_mut(key).and_then(Value::as_array_mut) {
            a.retain(|x| ns.iter().any(|n| x["namespace"].as_str() == Some(n.as_str())));
        }
    }
    if let Some(a) = v.get_mut("namespaces").and_then(Value::as_array_mut) {
        a.retain(|x| ns.iter().any(|n| x["name"].as_str() == Some(n.as_str())));
    }
    v["nodes"] = json!([]);
    v["restricted"] = json!(true);
    v
}

/// Summary of every cluster for the `clusters` topic.
pub fn publish_clusters(app: &App) {
    let clusters = app.store.read(|d| d.clusters.clone());
    let list: Vec<Value> = clusters
        .iter()
        .map(|c| {
            let snap = app.hub.latest(&format!("cluster:{}", c.name));
            let count = |k: &str| snap.as_ref().and_then(|s| s[k].as_array().map(Vec::len)).unwrap_or(0);
            let pods = snap.as_ref().and_then(|s| s["pods"].as_array().cloned()).unwrap_or_default();
            let nodes = snap.as_ref().and_then(|s| s["nodes"].as_array().cloned()).unwrap_or_default();
            json!({
                "name": c.name, "kind": c.kind, "status": c.status, "message": c.message, "endpoint": c.endpoint,
                "master": c.master, "workers": c.workers, "dns": c.dns, "created": c.created_at, "createdBy": c.created_by,
                "reachable": snap.as_ref().map(|s| s["reachable"] == true).unwrap_or(false),
                "nodes": nodes.len(), "nodesReady": nodes.iter().filter(|n| n["ready"] == true).count(),
                "pods": pods.len(), "podsRunning": pods.iter().filter(|p| p["status"] == "Running").count(),
                "podsPending": pods.iter().filter(|p| ["Pending", "ContainerCreating"].contains(&p["status"].as_str().unwrap_or(""))).count(),
                "podsFailed": pods.iter().filter(|p| ["Failed", "Error", "CrashLoopBackOff", "ImagePullBackOff", "ErrImagePull"].contains(&p["status"].as_str().unwrap_or(""))).count(),
                "deployments": count("deployments"), "services": count("services"), "namespaces": count("namespaces"),
                "ports": c.ports,
            })
        })
        .collect();
    app.hub.publish("clusters", Value::Array(list));
}

// ---------------------------------------------------------------------------
// Provisioning
// ---------------------------------------------------------------------------

async fn run_local(job: &JobHandle, program: &Path, args: &[String]) -> Result<String> {
    let out = tokio::process::Command::new(program).args(args).output().await.with_context(|| format!("running {}", program.display()))?;
    for l in String::from_utf8_lossy(&out.stdout).lines() {
        job.log(format!("  {l}"));
    }
    if !out.status.success() {
        bail!("{} failed: {}", program.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A machine's absolute install prefix (from the installer's facts).
fn prefix_of(m: &Machine) -> Result<String> {
    m.facts.get("prefix").and_then(Value::as_str).map(String::from).ok_or_else(|| anyhow!("machine {} is not installed yet", m.name))
}

fn write(path: &Path, body: &str) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(path, body)?;
    Ok(())
}

/// One service script: logs to the cluster's log directory, then `exec`s the command.
fn service(dir: &Path, cd: &str, name: &str, body: &str) -> Result<()> {
    write(
        &dir.join("services").join(format!("{name}.sh")),
        &format!("#!/bin/sh\nmkdir -p {cd}/logs\nexec >>{cd}/logs/{name}.log 2>&1\n{body}\n"),
    )
}

pub struct NodePlan {
    pub machine: Machine,
    pub node: String,
    pub master: bool,
}

/// Writes the payload directory for one machine.
fn payload(app: &App, c: &Cluster, plan: &NodePlan, master: &Machine) -> Result<PathBuf> {
    let src = app.store.cluster_dir(&c.name);
    let dir = src.join("payload").join(&plan.node);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(dir.join("pki"))?;
    let prefix = prefix_of(&plan.machine)?;
    let cd = format!("{prefix}/clusters/{}", c.name);
    let bin = format!("{prefix}/bin");
    let addr = &plan.machine.address;
    let p = &c.ports;
    let mut order = vec![];
    if plan.master {
        for f in [
            "ca.crt",
            "apiserver.crt",
            "apiserver.key",
            "apiserver-kubelet-client.crt",
            "apiserver-kubelet-client.key",
            "sa.key",
            "sa.pub",
            "kubelet.crt",
            "kubelet.key",
        ] {
            std::fs::copy(src.join("pki").join(f), dir.join("pki").join(f)).with_context(|| format!("copying {f}"))?;
        }
        for f in ["controller-manager.conf", "scheduler.conf", "kubelet.conf", "kube-proxy.conf"] {
            std::fs::copy(src.join(f), dir.join(f))?;
        }
        service(
            &dir,
            &cd,
            "kube-apiserver",
            &format!(
                "exec {bin}/kube-apiserver --bind-address {addr} --advertise-address {addr} --port {} --data-dir {cd}/data \\\n  --tls-cert-file {cd}/pki/apiserver.crt --tls-private-key-file {cd}/pki/apiserver.key --client-ca-file {cd}/pki/ca.crt \\\n  --authorization-mode Node,RBAC --service-account-key-file {cd}/pki/sa.pub --service-account-signing-key-file {cd}/pki/sa.key \\\n  --kubelet-client-certificate {cd}/pki/apiserver-kubelet-client.crt --kubelet-client-key {cd}/pki/apiserver-kubelet-client.key \\\n  --kubelet-certificate-authority {cd}/pki/ca.crt",
                p.api
            ),
        )?;
        service(
            &dir,
            &cd,
            "kube-controller-manager",
            &format!("exec {bin}/kube-controller-manager --kubeconfig {cd}/controller-manager.conf --root-ca-file {cd}/pki/ca.crt --bind-address 127.0.0.1 --port {}", p.controller_manager),
        )?;
        service(
            &dir,
            &cd,
            "kube-scheduler",
            &format!("exec {bin}/kube-scheduler --kubeconfig {cd}/scheduler.conf --bind-address 127.0.0.1 --port {}", p.scheduler),
        )?;
        order.extend(["kube-apiserver", "kube-controller-manager", "kube-scheduler"]);
    } else {
        let creds = src.join("nodes").join(&plan.node);
        for f in ["ca.crt", "kubelet.crt", "kubelet.key"] {
            std::fs::copy(creds.join(f), dir.join("pki").join(f))?;
        }
        std::fs::copy(creds.join("kubelet.conf"), dir.join("kubelet.conf"))?;
        std::fs::copy(src.join("kube-proxy.conf"), dir.join("kube-proxy.conf"))?;
    }
    let dns = if c.dns { format!(" --cluster-dns {CLUSTER_DNS}") } else { String::new() };
    service(
        &dir,
        &cd,
        "kubelet",
        &format!(
            "exec {bin}/kubelet --kubeconfig {cd}/kubelet.conf --hostname-override {} --node-ip {addr} --port {} --root-dir {cd}/kubelet \\\n  --tls-cert-file {cd}/pki/kubelet.crt --tls-private-key-file {cd}/pki/kubelet.key --client-ca-file {cd}/pki/ca.crt --authorization-mode Webhook{dns}",
            plan.node, p.kubelet
        ),
    )?;
    order.push("kubelet");
    if c.kube_proxy {
        let cname = format!("rustkube-proxy-{}-{}", c.name, plan.node);
        let table = format!("rustkube-{}-{}", c.name, plan.node);
        service(
            &dir,
            &cd,
            "kube-proxy",
            &format!(
                "docker rm -f {cname} >/dev/null 2>&1 || true\nCIDR=$(docker network inspect bridge -f '{{{{(index .IPAM.Config 0).Subnet}}}}')\nexec docker run --rm --name {cname} --network host --cap-add NET_ADMIN --cap-add NET_RAW \\\n  -v {bin}/kube-proxy:/usr/local/bin/kube-proxy:ro -v {cd}/kube-proxy.conf:/etc/kubernetes/kubeconfig:ro {KUBE_PROXY_IMAGE} \\\n  --kubeconfig /etc/kubernetes/kubeconfig --hostname-override {} --cluster-cidr \"$CIDR\" --nft-table {table} --hairpin-bridge docker0 \\\n  --healthz-bind-address 127.0.0.1:{} --metrics-bind-address 127.0.0.1:{}",
                plan.node, p.proxy_healthz, p.proxy_metrics
            ),
        )?;
        write(
            &dir.join("services/kube-proxy.stop.sh"),
            &format!(
                "#!/bin/sh\ndocker rm -f {cname} >/dev/null 2>&1 || true\ndocker run --rm --network host --cap-add NET_ADMIN -v {bin}/kube-proxy:/usr/local/bin/kube-proxy:ro {KUBE_PROXY_IMAGE} --cleanup --nft-table {table} >/dev/null 2>&1 || true\n"
            ),
        )?;
        order.push("kube-proxy");
    }
    write(&dir.join("services/order"), &(order.join("\n") + "\n"))?;
    write(&dir.join("NODE"), &format!("{}\n", plan.node))?;
    let _ = master;
    Ok(dir)
}

/// Copies a payload to a machine and starts its services.
async fn deploy(app: &App, c: &Cluster, plan: &NodePlan, dir: &Path, job: &JobHandle) -> Result<()> {
    let target = app.ssh.target(&plan.machine)?;
    let prefix = prefix_of(&plan.machine)?;
    let tarball = dir.with_extension("tar.gz");
    let out = tokio::process::Command::new("tar").arg("-czf").arg(&tarball).arg("-C").arg(dir).arg(".").output().await?;
    if !out.status.success() {
        bail!("packing payload: {}", String::from_utf8_lossy(&out.stderr));
    }
    let remote = format!("/tmp/rustkube-{}-{}.tar.gz", c.name, plan.node);
    job.log(format!("→ {} ({}): copying certificates and service definitions", plan.machine.name, target.describe()));
    target.copy(&tarball, &remote).await?;
    let cd = format!("{prefix}/clusters/{}", c.name);
    let script = format!(
        "set -e\nmkdir -p {cd}\ntar -xzf {remote} -C {cd}\nrm -f {remote}\nchmod 600 {cd}/pki/*.key {cd}/*.conf 2>/dev/null || true\n{prefix}/bin/rustkube-node start {}\n",
        sh_quote(&c.name)
    );
    job.log(format!("→ {}: starting {} services", plan.machine.name, if plan.master { "control-plane and node" } else { "node" }));
    target.run(&script, Some(job)).await?;
    let _ = std::fs::remove_file(tarball);
    Ok(())
}

pub fn plans(app: &App, c: &Cluster) -> Result<Vec<NodePlan>> {
    let master = app.store.machine(&c.master).ok_or_else(|| anyhow!("master machine {} not found", c.master))?;
    let mut out = vec![NodePlan { node: dns_label(&master.name), machine: master, master: true }];
    for w in &c.workers {
        if *w == c.master {
            continue;
        }
        let m = app.store.machine(w).ok_or_else(|| anyhow!("worker machine {w} not found"))?;
        out.push(NodePlan { node: dns_label(&m.name), machine: m, master: false });
    }
    Ok(out)
}

/// `provision` job.
pub async fn provision(app: Arc<App>, name: String, job: JobHandle) -> Result<()> {
    let c = app.store.cluster(&name).ok_or_else(|| anyhow!("cluster {name} not found"))?;
    let plans = plans(&app, &c)?;
    for p in &plans {
        if p.machine.status != "ready" {
            bail!("machine {} is not ready (status {}): install it first", p.machine.name, p.machine.status);
        }
    }
    let master = plans[0].machine.clone();
    let ip: std::net::IpAddr =
        master.address.parse().map_err(|_| anyhow!("the master's address must be an IP address (got {:?})", master.address))?;
    let dir = app.store.cluster_dir(&name);
    let kubeadm = app.bundle.binaries.join("kubeadm");
    let endpoint = format!("https://{ip}:{}", c.ports.api);
    job.log(format!("cluster {name}: master {} ({ip}), {} worker(s), API {endpoint}", master.name, plans.len() - 1));
    job.log("generating the cluster CA, certificates and kubeconfigs (kubeadm init)");
    run_local(
        &job,
        &kubeadm,
        &[
            "init".into(),
            "--cert-dir".into(),
            dir.join("pki").display().to_string(),
            "--kubeconfig-dir".into(),
            dir.display().to_string(),
            "--apiserver-advertise-address".into(),
            ip.to_string(),
            "--apiserver-bind-port".into(),
            c.ports.api.to_string(),
            "--node-name".into(),
            plans[0].node.clone(),
            "--node-ip".into(),
            ip.to_string(),
            "--cluster-name".into(),
            name.clone(),
            "--apiserver-cert-extra-sans".into(),
            "127.0.0.1".into(),
        ],
    )
    .await?;
    for p in plans.iter().skip(1) {
        job.log(format!("issuing kubelet credentials for worker {}", p.node));
        run_local(
            &job,
            &kubeadm,
            &[
                "node-credentials".into(),
                "--cert-dir".into(),
                dir.join("pki").display().to_string(),
                "--node-name".into(),
                p.node.clone(),
                "--node-ip".into(),
                p.machine.address.clone(),
                "--out".into(),
                dir.join("nodes").join(&p.node).display().to_string(),
                "--control-plane-endpoint".into(),
                endpoint.clone(),
                "--cluster-name".into(),
                name.clone(),
            ],
        )
        .await?;
    }
    app.store.update_cluster(&name, |x| x.endpoint = endpoint.clone());
    // Control plane first.
    let d = payload(&app, &c, &plans[0], &master)?;
    deploy(&app, &c, &plans[0], &d, &job).await?;
    let client = admin_client(&app, &app.store.cluster(&name).unwrap())?;
    job.log("waiting for the API server");
    wait_for(&job, Duration::from_secs(60), || async { client.get_text("/readyz").await.map(|t| t.contains("ok")).unwrap_or(false) })
        .await
        .context("the API server did not become ready (see kube-apiserver.log on the master)")?;
    for p in plans.iter().skip(1) {
        let d = payload(&app, &c, p, &master)?;
        deploy(&app, &c, p, &d, &job).await?;
    }
    job.log(format!("waiting for {} node(s) to be Ready", plans.len()));
    let names: Vec<String> = plans.iter().map(|p| p.node.clone()).collect();
    let nodes_api: Api<Node> = Api::all(client.clone());
    wait_for(&job, Duration::from_secs(120), || async {
        let Ok(list) = nodes_api.list(&ListParams::default()).await else { return false };
        names.iter().all(|n| {
            list.items.iter().any(|x| {
                x.metadata.name.as_deref() == Some(n.as_str())
                    && x.status
                        .as_ref()
                        .and_then(|s| s.conditions.as_ref())
                        .is_some_and(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
            })
        })
    })
    .await
    .context("nodes did not become Ready (see kubelet.log on the machines)")?;
    if c.dns {
        job.log("deploying the CoreDNS add-on");
        apply_yaml(&client, COREDNS_YAML, "default").await?;
    }
    app.store.update_cluster(&name, |x| {
        x.status = "running".into();
        x.message.clear();
    });
    start_runtime(&app, &name);
    sync_grants(&app, Some(&name)).await;
    job.log(format!("cluster {name} is running: {endpoint}"));
    Ok(())
}

async fn wait_for<F, Fut>(job: &JobHandle, timeout: Duration, mut f: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = std::time::Instant::now();
    let mut last_note = 0;
    while start.elapsed() < timeout {
        if f().await {
            return Ok(());
        }
        let secs = start.elapsed().as_secs();
        if secs / 15 > last_note {
            last_note = secs / 15;
            job.log(format!("  still waiting ({secs}s)"));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    bail!("timed out after {}s", timeout.as_secs())
}

/// Runs `rustkube-node <cmd> <cluster>` on every machine of a managed cluster.
pub async fn node_command(app: &App, name: &str, cmd: &str, job: &JobHandle, ignore_errors: bool) -> Result<()> {
    let c = app.store.cluster(name).ok_or_else(|| anyhow!("cluster {name} not found"))?;
    let mut plans = plans(app, &c)?;
    if cmd == "stop" || cmd == "remove" {
        plans.reverse();
    }
    for p in plans {
        let r = async {
            let prefix = prefix_of(&p.machine)?;
            let t = app.ssh.target(&p.machine)?;
            job.log(format!("→ {}: rustkube-node {cmd} {name}", p.machine.name));
            t.run(&format!("{prefix}/bin/rustkube-node {cmd} {}\n", sh_quote(name)), Some(job)).await.map(|_| ())
        }
        .await;
        match r {
            Err(e) if ignore_errors => job.log(format!("  warning: {e:#}")),
            r => r?,
        }
    }
    Ok(())
}

/// Service states and recent logs of a cluster's machines (managed clusters).
pub async fn node_status(app: &App, name: &str) -> Result<Value> {
    let c = app.store.cluster(name).ok_or_else(|| anyhow!("cluster {name} not found"))?;
    let mut out = vec![];
    for p in plans(app, &c)? {
        let prefix = prefix_of(&p.machine)?;
        let t = app.ssh.target(&p.machine)?;
        let r = t.run(&format!("{prefix}/bin/rustkube-node status {}\n", sh_quote(name)), None).await;
        let services: Vec<Value> = match &r {
            Ok(o) => o
                .lines()
                .filter_map(|l| l.strip_prefix("SERVICE "))
                .map(|l| {
                    let mut it = l.split_whitespace();
                    json!({"name": it.next().unwrap_or(""), "state": it.next().unwrap_or("unknown")})
                })
                .collect(),
            Err(_) => vec![],
        };
        out.push(json!({"machine": p.machine.name, "machineId": p.machine.id, "node": p.node, "master": p.master, "services": services, "error": r.err().map(|e| e.to_string())}));
    }
    Ok(Value::Array(out))
}

pub async fn service_log(app: &App, name: &str, machine: &str, service: &str, lines: u32) -> Result<String> {
    let m = app.store.machine(machine).ok_or_else(|| anyhow!("machine not found"))?;
    let prefix = prefix_of(&m)?;
    if !service.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("invalid service name");
    }
    app.ssh.target(&m)?.run(&format!("{prefix}/bin/rustkube-node logs {} {service} {lines}\n", sh_quote(name)), None).await
}

// ---------------------------------------------------------------------------
// Import, apply, access
// ---------------------------------------------------------------------------

/// Registers an existing cluster from a kubeconfig (needs cluster-admin rights).
pub async fn import(app: &Arc<App>, name: &str, kubeconfig: &str, user: &str) -> Result<()> {
    let dir = app.store.cluster_dir(name);
    std::fs::create_dir_all(&dir)?;
    crate::transport::SshPaths::store_secret(&dir.join("kubeconfig"), kubeconfig)?;
    let cfg = clientcmd::load_config(&dir.join("kubeconfig"), None).map_err(|e| anyhow!("invalid kubeconfig: {e}"))?;
    let endpoint = cfg.server.clone();
    let client = Client::new(cfg)?;
    let version = client.get_json("/version").await.map_err(|e| anyhow!("cannot reach {endpoint}: {e}"))?;
    // The console needs to impersonate users and manage RBAC.
    let review = json!({"apiVersion": "authorization.k8s.io/v1", "kind": "SelfSubjectAccessReview",
        "spec": {"resourceAttributes": {"verb": "impersonate", "resource": "users"}}});
    let r = client.send_json(reqwest_method("POST"), "/apis/authorization.k8s.io/v1/selfsubjectaccessreviews", &review).await;
    if !matches!(r, Ok(ref v) if v["status"]["allowed"] == true) {
        bail!("the kubeconfig's user must be a cluster admin (it cannot impersonate users)");
    }
    app.store.write(|d| {
        d.clusters.push(Cluster {
            name: name.into(),
            kind: "imported".into(),
            status: "running".into(),
            message: format!("Kubernetes {}", version["gitVersion"].as_str().unwrap_or("?")),
            endpoint,
            created_at: crate::state::now(),
            created_by: user.into(),
            ..Default::default()
        })
    });
    start_runtime(app, name);
    sync_grants(app, Some(name)).await;
    Ok(())
}

pub fn reqwest_method(m: &str) -> k8s_client_go::rest::Method {
    k8s_client_go::rest::Method::from_bytes(m.as_bytes()).unwrap_or(k8s_client_go::rest::Method::GET)
}

/// Every non-empty document of a multi-document YAML stream.
pub fn parse_yaml_docs(yaml: &str) -> Result<Vec<Value>> {
    let mut docs = vec![];
    for doc in serde_yaml::Deserializer::from_str(yaml) {
        let v: Value = serde::Deserialize::deserialize(doc).map_err(|e| anyhow!("invalid YAML: {e}"))?;
        if !v.is_null() {
            docs.push(v);
        }
    }
    Ok(docs)
}

/// Server-side applies every document of `yaml`; returns one line per object.
pub async fn apply_yaml(client: &Client, yaml: &str, default_ns: &str) -> Result<Vec<String>> {
    let resources = k8s_client_go::discovery::server_preferred_resources(client).await.map_err(|e| anyhow!("discovery: {e}"))?;
    let mut out = vec![];
    for v in parse_yaml_docs(yaml)? {
        let items = if v["kind"].as_str().is_some_and(|k| k.ends_with("List")) {
            v["items"].as_array().cloned().unwrap_or_default()
        } else {
            vec![v]
        };
        for mut obj in items {
            let api_version = obj["apiVersion"].as_str().unwrap_or("").to_string();
            let kind = obj["kind"].as_str().unwrap_or("").to_string();
            let name = obj.pointer("/metadata/name").and_then(Value::as_str).unwrap_or("").to_string();
            if kind.is_empty() || name.is_empty() {
                bail!("every object needs apiVersion, kind and metadata.name");
            }
            let r = resources
                .iter()
                .find(|r| r.resource.api_version() == api_version && r.resource.kind == kind)
                .ok_or_else(|| anyhow!("the cluster does not serve {kind} in {api_version}"))?;
            let ns = if r.resource.namespaced {
                let ns = obj.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or(default_ns).to_string();
                obj["metadata"]["namespace"] = json!(ns);
                Some(ns)
            } else {
                None
            };
            let path = format!("{}/{name}?fieldManager=rustkubeadmin&force=true", r.resource.collection_path(ns.as_deref()));
            client
                .request_raw(reqwest_method("PATCH"), &path, "application/apply-patch+yaml", serde_json::to_vec(&obj)?)
                .await
                .map_err(|e| anyhow!("{kind} {name}: {e}"))?;
            out.push(format!("{}/{name} applied{}", kind.to_lowercase(), ns.map(|n| format!(" in {n}")).unwrap_or_default()));
        }
    }
    Ok(out)
}

/// ClusterRole a console grant maps to.
fn grant_role(role: &str, cluster_wide: bool) -> &'static str {
    match (role, cluster_wide) {
        ("admin", true) => "cluster-admin",
        ("admin", false) => "admin",
        ("edit", _) => "edit",
        _ => "view",
    }
}

/// Makes the RBAC bindings in `only` (or every running cluster) match the users' grants.
pub async fn sync_grants(app: &App, only: Option<&str>) {
    let (users, clusters) = app.store.read(|d| (d.users.clone(), d.clusters.clone()));
    for c in clusters.iter().filter(|c| c.status == "running" && only.is_none_or(|o| o == c.name)) {
        let Ok(client) = admin_client(app, c) else { continue };
        if let Err(e) = sync_cluster_grants(&client, c, &users).await {
            tracing::warn!(cluster = %c.name, "syncing access: {e:#}");
        }
    }
}

async fn sync_cluster_grants(client: &Client, c: &Cluster, users: &[crate::state::User]) -> Result<()> {
    // desired: (namespace, name) → binding
    let mut want: BTreeMap<(String, String), Value> = BTreeMap::new();
    for u in users.iter().filter(|u| !u.disabled) {
        for g in u.grants.iter().filter(|g| g.cluster == c.name) {
            let name = format!("{KUBE_USER_PREFIX}{}:{}", u.username, g.role);
            let wide = g.namespace.is_empty();
            let mut b = json!({
                "apiVersion": "rbac.authorization.k8s.io/v1",
                "kind": if wide { "ClusterRoleBinding" } else { "RoleBinding" },
                "metadata": {"name": name, "labels": {MANAGED_LABEL: "true", "rustkubeadmin.io/user": u.username}},
                "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": grant_role(&g.role, wide)},
                "subjects": [{"apiGroup": "rbac.authorization.k8s.io", "kind": "User", "name": format!("{KUBE_USER_PREFIX}{}", u.username)}],
            });
            if !wide {
                b["metadata"]["namespace"] = json!(g.namespace);
            }
            want.insert((g.namespace.clone(), name), b);
        }
    }
    // Everyone signed in through the console may discover APIs and review their own access.
    let sel = format!("labelSelector={MANAGED_LABEL}%3Dtrue");
    let mut have: HashMap<(String, String), ()> = HashMap::new();
    for (path, wide) in
        [("/apis/rbac.authorization.k8s.io/v1/clusterrolebindings", true), ("/apis/rbac.authorization.k8s.io/v1/rolebindings", false)]
    {
        let list = client.get_json(&format!("{path}?{sel}")).await.map_err(|e| anyhow!("{e}"))?;
        for item in list["items"].as_array().into_iter().flatten() {
            let ns =
                if wide { String::new() } else { item.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or("").to_string() };
            let name = item.pointer("/metadata/name").and_then(Value::as_str).unwrap_or("").to_string();
            if want.contains_key(&(ns.clone(), name.clone())) {
                have.insert((ns, name), ());
                continue;
            }
            let del = if wide {
                format!("{path}/{name}")
            } else {
                format!("/apis/rbac.authorization.k8s.io/v1/namespaces/{ns}/rolebindings/{name}")
            };
            let _ = client.request(reqwest_method("DELETE"), &del, None).await;
            tracing::info!(cluster = %c.name, binding = %name, "removed access binding");
        }
    }
    for ((ns, name), b) in want {
        if have.contains_key(&(ns.clone(), name.clone())) {
            continue;
        }
        if !ns.is_empty() {
            // The namespace may not exist yet: create it so the grant takes effect.
            let _ = client
                .send_json(
                    reqwest_method("POST"),
                    "/api/v1/namespaces",
                    &json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": ns}}),
                )
                .await;
        }
        let path = if ns.is_empty() {
            "/apis/rbac.authorization.k8s.io/v1/clusterrolebindings".to_string()
        } else {
            format!("/apis/rbac.authorization.k8s.io/v1/namespaces/{ns}/rolebindings")
        };
        client.send_json(reqwest_method("POST"), &path, &b).await.map_err(|e| anyhow!("creating {name}: {e}"))?;
        tracing::info!(cluster = %c.name, binding = %name, namespace = %ns, "granted access");
    }
    let _ = KUBE_GROUP;
    Ok(())
}

/// Kubeconfig for the signed-in user (managed clusters: a client certificate from the cluster CA).
pub async fn user_kubeconfig(app: &App, c: &Cluster, username: &str, superadmin: bool) -> Result<String> {
    let dir = app.store.cluster_dir(&c.name);
    if c.kind == "imported" {
        if superadmin {
            return Ok(std::fs::read_to_string(dir.join("kubeconfig"))?);
        }
        bail!("kubeconfigs for imported clusters are only available to the super admin");
    }
    if superadmin {
        return Ok(std::fs::read_to_string(dir.join("admin.conf"))?);
    }
    let kubeadm = app.bundle.binaries.join("kubeadm");
    let out = tokio::process::Command::new(&kubeadm)
        .args(["kubeconfig", "user", "--client-name"])
        .arg(format!("{KUBE_USER_PREFIX}{username}"))
        .args(["--org", KUBE_GROUP, "--cert-dir"])
        .arg(dir.join("pki"))
        .args(["--control-plane-endpoint", &c.endpoint, "--cluster-name", &c.name])
        .output()
        .await?;
    if !out.status.success() {
        bail!("kubeadm: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
