//! Port of `pkg/kubemark`: **hollow nodes**.
//!
//! A hollow node registers a `Node` with realistic capacity and labels, renews
//! its `kube-node-lease` Lease, and plays the kubelet's part for pods bound to
//! it — without running any containers:
//!
//! * a newly bound pod becomes `Running` and `Ready` (with a pod IP from the
//!   node's `podCIDR`) after `startup_delay`;
//! * pods with `restartPolicy: Never/OnFailure` (e.g. Jobs) finish as
//!   `Succeeded` after `job_duration`;
//! * pods marked for deletion are removed (`gracePeriodSeconds=0`), as the
//!   kubelet does after stopping containers.
//!
//! This lets the control plane (scheduler, controllers, endpoints) be exercised
//! end-to-end before the real kubelet exists, exactly like upstream kubemark.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use k8s_api::coordination::v1::{Lease, LeaseSpec};
use k8s_api::core::v1::{ContainerState, ContainerStateRunning, ContainerStateTerminated, ContainerStatus, Node, Pod, PodCondition, PodIp};
use k8s_apimachinery::meta::{now_rfc3339, ObjectMeta};
use k8s_client_go::tools::cache::{Event, Informer};
use k8s_client_go::{Api, Client, ListParams};
use serde_json::json;

#[derive(Clone, Debug)]
pub struct HollowConfig {
    pub name_prefix: String,
    pub count: usize,
    pub cpu: String,
    pub memory: String,
    pub max_pods: u32,
    /// Spread nodes over this many zones (`topology.kubernetes.io/zone=zone-N`).
    pub zones: usize,
    pub extra_labels: BTreeMap<String, String>,
    pub startup_delay: Duration,
    pub job_duration: Duration,
    pub lease_duration_secs: i32,
}

impl Default for HollowConfig {
    fn default() -> Self {
        Self {
            name_prefix: "hollow-node".into(),
            count: 3,
            cpu: "4".into(),
            memory: "8Gi".into(),
            max_pods: 110,
            zones: 1,
            extra_labels: BTreeMap::new(),
            startup_delay: Duration::from_millis(500),
            job_duration: Duration::from_secs(5),
            lease_duration_secs: 40,
        }
    }
}

pub struct HollowNodes {
    client: Client,
    cfg: HollowConfig,
    /// node name → index
    nodes: HashMap<String, usize>,
    handled: Mutex<HashSet<String>>,
    next_ip: Mutex<HashMap<usize, u32>>,
}

fn micro(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

impl HollowNodes {
    pub fn new(client: Client, cfg: HollowConfig) -> Arc<Self> {
        let nodes = (0..cfg.count).map(|i| (format!("{}-{i}", cfg.name_prefix), i)).collect();
        Arc::new(Self { client, cfg, nodes, handled: Mutex::new(HashSet::new()), next_ip: Mutex::new(HashMap::new()) })
    }

    pub fn node_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.nodes.keys().cloned().collect();
        v.sort();
        v
    }

    fn node_object(&self, name: &str, i: usize) -> Node {
        let mut labels: BTreeMap<String, String> = [
            ("kubernetes.io/hostname", name.to_string()),
            ("kubernetes.io/os", "linux".into()),
            ("kubernetes.io/arch", "amd64".into()),
            ("node.kubernetes.io/instance-type", "hollow".into()),
            ("topology.kubernetes.io/zone", format!("zone-{}", i % self.cfg.zones.max(1))),
            ("topology.kubernetes.io/region", "hollow-region".into()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        labels.extend(self.cfg.extra_labels.clone());
        let mut node = Node::new(ObjectMeta { name: Some(name.into()), labels: Some(labels), ..Default::default() });
        node.spec = Some(k8s_api::core::v1::NodeSpec {
            pod_cidr: Some(format!("10.244.{}.0/24", i + 1)),
            pod_cidrs: Some(vec![format!("10.244.{}.0/24", i + 1)]),
            provider_id: Some(format!("kubemark://{name}")),
            ..Default::default()
        });
        node
    }

    fn node_status(&self, i: usize) -> serde_json::Value {
        let now = now_rfc3339();
        let resources =
            json!({"cpu": self.cfg.cpu, "memory": self.cfg.memory, "pods": self.cfg.max_pods.to_string(), "ephemeral-storage": "100Gi"});
        let cond = |t: &str, s: &str, reason: &str, msg: &str| json!({"type": t, "status": s, "reason": reason, "message": msg, "lastHeartbeatTime": now, "lastTransitionTime": now});
        json!({
            "capacity": resources, "allocatable": resources,
            "conditions": [
                cond("MemoryPressure", "False", "KubeletHasSufficientMemory", "kubelet has sufficient memory available"),
                cond("DiskPressure", "False", "KubeletHasNoDiskPressure", "kubelet has no disk pressure"),
                cond("PIDPressure", "False", "KubeletHasSufficientPID", "kubelet has sufficient PID available"),
                cond("Ready", "True", "KubeletReady", "kubelet is posting ready status (hollow)"),
            ],
            "addresses": [{"type": "InternalIP", "address": format!("10.0.0.{}", i + 10)}, {"type": "Hostname", "address": self.node_names().get(i).cloned().unwrap_or_default()}],
            "daemonEndpoints": {"kubeletEndpoint": {"Port": 10250}},
            "nodeInfo": {
                "machineID": format!("hollow-{i}"), "systemUUID": format!("hollow-{i}"), "bootID": format!("hollow-{i}"),
                "kernelVersion": "hollow", "osImage": "rustkubernetes hollow node", "containerRuntimeVersion": "hollow://0.1.0",
                "kubeletVersion": "v1.35.0-hollow", "kubeProxyVersion": "v1.35.0-hollow", "operatingSystem": "linux", "architecture": "amd64",
            },
        })
    }

    /// Creates (or adopts existing) nodes and marks them Ready.
    pub async fn register(&self) -> anyhow::Result<()> {
        let api: Api<Node> = Api::all(self.client.clone());
        for (name, &i) in &self.nodes {
            let node = match api.get_opt(name).await? {
                Some(n) => n,
                None => api.create(&self.node_object(name, i)).await?,
            };
            let mut v = serde_json::to_value(&node)?;
            v["status"] = self.node_status(i);
            api.dynamic().replace_status(name, &v).await?;
            tracing::info!(node = %name, "registered hollow node");
        }
        Ok(())
    }

    /// Renews every node's Lease (`kube-node-lease`), creating it if needed.
    pub async fn heartbeat(&self) -> anyhow::Result<()> {
        let leases: Api<Lease> = Api::namespaced(self.client.clone(), "kube-node-lease");
        let now = micro(chrono::Utc::now());
        for name in self.nodes.keys() {
            match leases.get_opt(name).await? {
                Some(mut l) => {
                    l.spec.get_or_insert_with(Default::default).renew_time = Some(now.clone());
                    let _ = leases.replace(&l).await;
                }
                None => {
                    let mut l = Lease::new(ObjectMeta::namespaced("kube-node-lease", name));
                    l.spec = Some(LeaseSpec {
                        holder_identity: Some(name.clone()),
                        lease_duration_seconds: Some(self.cfg.lease_duration_secs),
                        renew_time: Some(now.clone()),
                        ..Default::default()
                    });
                    let _ = leases.create(&l).await;
                }
            }
        }
        Ok(())
    }

    fn alloc_ip(&self, node_idx: usize) -> String {
        let mut m = self.next_ip.lock().unwrap();
        let n = m.entry(node_idx).or_insert(1);
        *n = if *n >= 254 { 2 } else { *n + 1 };
        format!("10.244.{}.{}", node_idx + 1, n)
    }

    fn running_status(&self, pod: &Pod, node_idx: usize) -> Pod {
        let now = now_rfc3339();
        let mut p = pod.clone();
        let ip = self.alloc_ip(node_idx);
        let spec = p.spec.clone().unwrap_or_default();
        let uid = p.metadata.uid.clone().unwrap_or_default();
        let st = p.status.get_or_insert_with(Default::default);
        st.phase = Some("Running".into());
        st.host_ip = Some(format!("10.0.0.{}", node_idx + 10));
        st.pod_ip = Some(ip.clone());
        st.pod_ips = Some(vec![PodIp { ip }]);
        st.start_time = Some(now.clone());
        st.container_statuses = Some(
            spec.containers
                .iter()
                .map(|c| ContainerStatus {
                    name: c.name.clone(),
                    state: Some(ContainerState {
                        running: Some(ContainerStateRunning { started_at: Some(now.clone()) }),
                        ..Default::default()
                    }),
                    ready: true,
                    restart_count: 0,
                    image: c.image.clone(),
                    image_id: c.image.clone().map(|i| format!("hollow://{i}")),
                    container_id: Some(format!("hollow://{uid}-{}", c.name)),
                    started: Some(true),
                    ..Default::default()
                })
                .collect(),
        );
        if let Some(inits) = &spec.init_containers {
            st.init_container_statuses = Some(
                inits
                    .iter()
                    .map(|c| ContainerStatus {
                        name: c.name.clone(),
                        state: Some(ContainerState {
                            terminated: Some(ContainerStateTerminated {
                                exit_code: 0,
                                reason: Some("Completed".into()),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        ready: true,
                        image: c.image.clone(),
                        ..Default::default()
                    })
                    .collect(),
            );
        }
        let cond = |t: &str| PodCondition {
            type_: t.into(),
            status: "True".into(),
            last_transition_time: Some(now.clone()),
            ..Default::default()
        };
        st.conditions = Some(vec![
            cond("PodReadyToStartContainers"),
            cond("Initialized"),
            cond("ContainersReady"),
            cond("Ready"),
            cond("PodScheduled"),
        ]);
        p
    }

    fn succeeded_status(pod: &Pod) -> Pod {
        let now = now_rfc3339();
        let mut p = pod.clone();
        let st = p.status.get_or_insert_with(Default::default);
        st.phase = Some("Succeeded".into());
        for cs in st.container_statuses.iter_mut().flatten() {
            cs.ready = false;
            cs.started = Some(false);
            cs.state = Some(ContainerState {
                terminated: Some(ContainerStateTerminated {
                    exit_code: 0,
                    reason: Some("Completed".into()),
                    finished_at: Some(now.clone()),
                    container_id: cs.container_id.clone(),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
        for c in st.conditions.iter_mut().flatten() {
            if c.type_ == "Ready" || c.type_ == "ContainersReady" {
                c.status = "False".into();
                c.reason = Some("PodCompleted".into());
                c.last_transition_time = Some(now.clone());
            }
        }
        p
    }

    async fn handle(self: &Arc<Self>, pod: Arc<Pod>) {
        let Some(node) = pod.spec.as_ref().and_then(|s| s.node_name.clone()) else { return };
        let Some(&idx) = self.nodes.get(&node) else { return };
        let uid = pod.metadata.uid.clone().unwrap_or_default();
        let api: Api<Pod> = Api::namespaced(self.client.clone(), pod.metadata.namespace());
        let name = pod.metadata.name().to_string();

        if pod.metadata.deletion_timestamp.is_some() {
            if !self.handled.lock().unwrap().insert(format!("del:{uid}")) {
                return;
            }
            // "Stop containers" then confirm the deletion.
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = api.dynamic().delete(&name, Some(0)).await;
            return;
        }
        let phase = pod.status.as_ref().and_then(|s| s.phase.clone()).unwrap_or_else(|| "Pending".into());
        if phase != "Pending" || !self.handled.lock().unwrap().insert(format!("run:{uid}")) {
            return;
        }
        tokio::time::sleep(self.cfg.startup_delay).await;
        let Ok(Some(cur)) = api.get_opt(&name).await else { return };
        if cur.metadata.uid != pod.metadata.uid || cur.metadata.deletion_timestamp.is_some() {
            return;
        }
        if let Err(e) = api.replace_status(&self.running_status(&cur, idx)).await {
            tracing::debug!(pod = %name, "status update failed: {e}");
            self.handled.lock().unwrap().remove(&format!("run:{uid}"));
            return;
        }
        let restart = cur.spec.as_ref().and_then(|s| s.restart_policy.clone()).unwrap_or_else(|| "Always".into());
        if restart != "Always" {
            tokio::time::sleep(self.cfg.job_duration).await;
            if let Ok(Some(cur)) = api.get_opt(&name).await {
                if cur.metadata.uid == pod.metadata.uid && cur.metadata.deletion_timestamp.is_none() {
                    let _ = api.replace_status(&Self::succeeded_status(&cur)).await;
                }
            }
        }
    }

    /// Registers the nodes and serves them until the process stops.
    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        self.register().await?;
        let me = self.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_secs(10));
            let mut n = 0u64;
            loop {
                t.tick().await;
                if let Err(e) = me.heartbeat().await {
                    tracing::warn!("lease renewal failed: {e}");
                }
                n += 1;
                if n.is_multiple_of(30) {
                    let _ = me.register().await; // refresh node status every 5 minutes
                }
            }
        });
        let inf = Informer::new(Api::<Pod>::all(self.client.clone()), ListParams::default());
        let mut rx = inf.subscribe();
        inf.start();
        loop {
            match rx.recv().await {
                Ok(Event::Added(p)) | Ok(Event::Updated { new: p, .. }) => {
                    let me = self.clone();
                    tokio::spawn(async move { me.handle(p).await });
                }
                Ok(Event::Deleted(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    for p in inf.store().list() {
                        let me = self.clone();
                        tokio::spawn(async move { me.handle(p).await });
                    }
                }
                Err(_) => return Ok(()),
            }
        }
    }
}
