//! Node registration, status and Lease heartbeats (`kubelet_node_status.go`,
//! `nodestatus/setters.go`, `component-helpers/apimachinery/lease`).

use std::collections::BTreeMap;

use anyhow::Result;
use k8s_api::coordination::v1::{Lease, LeaseSpec};
use k8s_api::core::v1::Node;
use k8s_apimachinery::meta::{now_rfc3339, ObjectMeta};
use k8s_client_go::Api;
use serde_json::{json, Value};

use crate::{KubeletConfig, KubeletCtx};

pub const KUBELET_VERSION: &str = "v1.35.0-rustkubernetes";
const LEASE_DURATION: i32 = 40;
/// `volutil.ControllerManagedAttachAnnotation`
pub const CONTROLLER_MANAGED_ATTACH_ANNOTATION: &str = "volumes.kubernetes.io/controller-managed-attach-detach";

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// MemTotal from /proc/meminfo, as a quantity string (`16384000Ki`).
fn memory() -> String {
    read("/proc/meminfo")
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:").map(|v| v.trim().trim_end_matches("kB").trim().to_string()))
        .map(|kb| format!("{kb}Ki"))
        .unwrap_or_else(|| "0".into())
}

fn os_image() -> String {
    read("/etc/os-release")
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string()))
        .unwrap_or_else(|| "Linux".into())
}

fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// Free/total bytes of the filesystem holding `path`.
fn disk_capacity(path: &std::path::Path) -> u64 {
    let c = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap_or_default();
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0 {
        st.f_blocks * st.f_frsize
    } else {
        0
    }
}

fn status(ctx: &KubeletCtx, cfg: &KubeletConfig, runtime_version: &str, images: Vec<Value>) -> Value {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let resources = json!({
        "cpu": cpus.to_string(),
        "memory": memory(),
        "pods": cfg.max_pods.to_string(),
        "ephemeral-storage": disk_capacity(&ctx.root).to_string(),
    });
    let now = now_rfc3339();
    let cond = |t: &str, s: &str, reason: &str, msg: &str| json!({"type": t, "status": s, "reason": reason, "message": msg, "lastHeartbeatTime": now, "lastTransitionTime": now});
    json!({
        "capacity": resources,
        "allocatable": resources,
        "conditions": [
            cond("MemoryPressure", "False", "KubeletHasSufficientMemory", "kubelet has sufficient memory available"),
            cond("DiskPressure", "False", "KubeletHasNoDiskPressure", "kubelet has no disk pressure"),
            cond("PIDPressure", "False", "KubeletHasSufficientPID", "kubelet has sufficient PID available"),
            ready_condition(ctx, &cond),
        ],
        "addresses": [{"type": "InternalIP", "address": cfg.node_ip}, {"type": "Hostname", "address": cfg.node_name}],
        "daemonEndpoints": {"kubeletEndpoint": {"Port": cfg.port}},
        "nodeInfo": {
            "machineID": read("/etc/machine-id").trim(),
            "systemUUID": read("/etc/machine-id").trim(),
            "bootID": read("/proc/sys/kernel/random/boot_id").trim(),
            "kernelVersion": read("/proc/sys/kernel/osrelease").trim(),
            "osImage": os_image(),
            "containerRuntimeVersion": runtime_version,
            "kubeletVersion": KUBELET_VERSION,
            "kubeProxyVersion": KUBELET_VERSION,
            "operatingSystem": "linux",
            "architecture": arch(),
        },
        "images": images,
    })
}

/// Conditions the kubelet owns; others (e.g. `NetworkUnavailable`, set by the
/// network add-on) are kept as they are.
const KUBELET_CONDITIONS: &[&str] = &["MemoryPressure", "DiskPressure", "PIDPressure", "Ready"];

/// `nodestatus.ReadyCondition`: not ready while the runtime or its network is not.
fn ready_condition(ctx: &KubeletCtx, cond: &dyn Fn(&str, &str, &str, &str) -> Value) -> Value {
    let errs: Vec<String> = [ctx.runtime_state.runtime_error(), ctx.runtime_state.network_error()].into_iter().flatten().collect();
    match errs.len() {
        0 => cond("Ready", "True", "KubeletReady", "kubelet is posting ready status"),
        // errors.NewAggregate
        1 => cond("Ready", "False", "KubeletNotReady", &errs[0]),
        _ => cond("Ready", "False", "KubeletNotReady", &format!("[{}]", errs.join(", "))),
    }
}

async fn images(ctx: &KubeletCtx) -> Vec<Value> {
    let mut list = ctx.runtime.list_images().await.unwrap_or_default();
    list.sort_by_key(|i| std::cmp::Reverse(i.1));
    list.into_iter().filter(|(n, _)| !n.is_empty()).take(50).map(|(names, size)| json!({"names": names, "sizeBytes": size})).collect()
}

/// `registerWithAPIServer` + initial status.
pub async fn register(ctx: &KubeletCtx, cfg: &KubeletConfig, runtime_version: &str) -> Result<()> {
    let api: Api<Node> = Api::all(ctx.client.clone());
    let mut labels: BTreeMap<String, String> = [
        ("kubernetes.io/hostname", cfg.node_name.clone()),
        ("kubernetes.io/os", "linux".to_string()),
        ("kubernetes.io/arch", arch().to_string()),
        ("beta.kubernetes.io/os", "linux".to_string()),
        ("beta.kubernetes.io/arch", arch().to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    labels.extend(cfg.node_labels.iter().cloned());
    // --enable-controller-attach-detach (default true): volumes are attached by the controller manager.
    let annotations: BTreeMap<String, String> = [(CONTROLLER_MANAGED_ATTACH_ANNOTATION.to_string(), "true".to_string())].into();
    let node = match api.get_opt(&cfg.node_name).await? {
        Some(mut n) => {
            n.metadata.labels.get_or_insert_with(BTreeMap::new).extend(labels);
            n.metadata.annotations.get_or_insert_with(BTreeMap::new).extend(annotations);
            api.replace(&n).await?
        }
        None => {
            let mut n = Node::new(ObjectMeta {
                name: Some(cfg.node_name.clone()),
                labels: Some(labels),
                annotations: Some(annotations),
                ..Default::default()
            });
            if !cfg.register_taints.is_empty() {
                n.spec = Some(k8s_api::core::v1::NodeSpec { taints: Some(cfg.register_taints.clone()), ..Default::default() });
            }
            let n = api.create(&n).await?;
            tracing::info!(node = %cfg.node_name, "registered node");
            n
        }
    };
    drop(node);
    update_status(ctx, cfg, runtime_version).await
}

/// Writes a fresh node status, retrying on conflicts with controllers that
/// update the node concurrently (`tryUpdateNodeStatus`, `nodeStatusUpdateRetry`).
async fn update_status(ctx: &KubeletCtx, cfg: &KubeletConfig, runtime_version: &str) -> Result<()> {
    let api: Api<Node> = Api::all(ctx.client.clone());
    let imgs = images(ctx).await;
    let mut last = None;
    for _ in 0..5 {
        let Some(node) = api.get_opt(&cfg.node_name).await? else {
            anyhow::bail!("node {} not found", cfg.node_name);
        };
        // Hand the node's pod CIDR to the runtime (`tryUpdateNodeStatus` → `updatePodCIDR`).
        let pod_cidrs = node.spec.as_ref().and_then(|s| s.pod_cidrs.clone()).unwrap_or_default().join(",");
        if !pod_cidrs.is_empty() {
            match crate::update_pod_cidr(ctx, &pod_cidrs).await {
                Ok(true) => crate::update_runtime_up(ctx).await,
                Ok(false) => {}
                Err(e) => tracing::error!("{e:#}"),
            }
        }
        let mut v = serde_json::to_value(&node)?;
        let mut st = status(ctx, cfg, runtime_version, imgs.clone());
        // Keep transition times of unchanged conditions.
        if let (Some(old), Some(new)) = (v["status"]["conditions"].as_array().cloned(), st["conditions"].as_array_mut()) {
            for c in new.iter_mut() {
                if let Some(o) = old.iter().find(|o| o["type"] == c["type"] && o["status"] == c["status"]) {
                    c["lastTransitionTime"] = o["lastTransitionTime"].clone();
                }
            }
            let was_ready = old.iter().find(|o| o["type"] == "Ready").map(|o| o["status"] == "True");
            let is_ready = new.iter().find(|c| c["type"] == "Ready").map(|c| c["status"] == "True");
            if was_ready.is_some() && was_ready != is_ready {
                let reason = if is_ready == Some(true) { "NodeReady" } else { "NodeNotReady" };
                ctx.recorder.event(&node, "Normal", reason, format!("Node {} status is now: {reason}", cfg.node_name));
            }
            for o in old.iter().filter(|o| !o["type"].as_str().is_some_and(|t| KUBELET_CONDITIONS.contains(&t))) {
                new.push(o.clone());
            }
        }
        v["status"] = st;
        match api.dynamic().replace_status(&cfg.node_name, &v).await {
            Ok(_) => return Ok(()),
            Err(e) if e.to_string().contains("modified") => last = Some(e),
            Err(e) => return Err(e.into()),
        }
    }
    Err(last.map(Into::into).unwrap_or_else(|| anyhow::anyhow!("node status update failed")))
}

/// Renews the node Lease and refreshes the node status.
pub async fn heartbeat(ctx: &KubeletCtx, cfg: &KubeletConfig, runtime_version: &str) -> Result<()> {
    let leases: Api<Lease> = Api::namespaced(ctx.client.clone(), "kube-node-lease");
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string();
    match leases.get_opt(&cfg.node_name).await? {
        Some(mut l) => {
            l.spec.get_or_insert_with(Default::default).renew_time = Some(now);
            leases.replace(&l).await?;
        }
        None => {
            let mut l = Lease::new(ObjectMeta::namespaced("kube-node-lease", &cfg.node_name));
            l.spec = Some(LeaseSpec {
                holder_identity: Some(cfg.node_name.clone()),
                lease_duration_seconds: Some(LEASE_DURATION),
                renew_time: Some(now),
                ..Default::default()
            });
            leases.create(&l).await?;
        }
    }
    let api: Api<Node> = Api::all(ctx.client.clone());
    if api.get_opt(&cfg.node_name).await?.is_none() {
        return register(ctx, cfg, runtime_version).await;
    }
    update_status(ctx, cfg, runtime_version).await
}
