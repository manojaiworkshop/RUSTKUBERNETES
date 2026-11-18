//! Port of `k8s.io/kubernetes/pkg/kubelet`: the node agent.
//!
//! | Module | Go equivalent |
//! |--------|---------------|
//! | [`runtime`] | CRI `RuntimeService` / `ImageService` (subset) |
//! | [`dns`] | `network/dns`: pod resolver settings |
//! | [`dockershim`] | `cri-dockerd` / former `pkg/kubelet/dockershim` |
//! | [`pod_workers`] | `pod_workers.go`, `kuberuntime/kuberuntime_manager.go` |
//! | [`status`] | `kubelet_pods.go: generateAPIPodStatus` |
//! | [`prober`] | `prober`, `pkg/probe` |
//! | [`volumes`] | `pkg/volume/*` (emptyDir, hostPath, configMap, secret, downwardAPI, projected) |
//! | [`envvars`] | `kubelet_pods.go: makeEnvironmentVariables` |
//! | [`nodestatus`] | `kubelet_node_status.go`, node lease |
//! | [`server`] | `pkg/kubelet/server` (logs, exec) |
//!
//! [`Kubelet::run`] registers the node, keeps its Lease fresh, watches pods
//! bound to this node and runs one [`pod_workers::PodWorker`] per pod. A
//! background relist (PLEG) refreshes the runtime cache every second.

pub mod dns;
pub mod dockershim;
pub mod envvars;
pub mod nodestatus;
pub mod pod_workers;
pub mod prober;
pub mod runtime;
pub mod server;
pub mod status;
pub mod volumes;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use k8s_api::core::v1::Pod;
use k8s_client_go::tools::cache::{Event, Informer, Store};
use k8s_client_go::tools::record::EventRecorder;
use k8s_client_go::{Api, Client, ListParams};
use tokio::sync::watch;

use runtime::{ContainerStatus, Runtime, SandboxStatus};

/// Kubelet settings (`KubeletConfiguration` subset + flags).
#[derive(Clone, Debug)]
pub struct KubeletConfig {
    pub node_name: String,
    /// Address the API server uses to reach this kubelet (and the pods' host IP).
    pub node_ip: String,
    pub root_dir: PathBuf,
    pub port: u16,
    pub max_pods: u32,
    pub node_labels: Vec<(String, String)>,
    pub register_taints: Vec<k8s_api::core::v1::Taint>,
    /// `--cluster-dns`
    pub cluster_dns: Vec<String>,
    /// `--cluster-domain`
    pub cluster_domain: String,
    /// HTTPS for the kubelet API (`--tls-cert-file` etc.); `None` serves plain HTTP.
    pub tls: Option<server::TlsServing>,
}

/// Last relist of the runtime (`kubecontainer.Cache` fed by the PLEG).
#[derive(Default)]
pub struct RuntimeCache {
    state: RwLock<(Vec<SandboxStatus>, Vec<ContainerStatus>)>,
    refresh: tokio::sync::Mutex<()>,
}

impl RuntimeCache {
    pub async fn refresh(&self, rt: &dyn Runtime) {
        let _g = self.refresh.lock().await;
        match (rt.list_pod_sandboxes().await, rt.list_containers().await) {
            (Ok(s), Ok(c)) => *self.state.write().unwrap() = (s, c),
            (Err(e), _) | (_, Err(e)) => tracing::warn!("runtime relist failed: {e:#}"),
        }
    }

    pub fn for_pod(&self, uid: &str) -> (Vec<SandboxStatus>, Vec<ContainerStatus>) {
        let st = self.state.read().unwrap();
        (st.0.iter().filter(|s| s.pod_uid == uid).cloned().collect(), st.1.iter().filter(|c| c.pod_uid == uid).cloned().collect())
    }

    pub fn all(&self) -> (Vec<SandboxStatus>, Vec<ContainerStatus>) {
        self.state.read().unwrap().clone()
    }
}

/// State shared by the kubelet, its workers and its server.
pub struct KubeletCtx {
    pub client: Client,
    pub runtime: Arc<dyn Runtime>,
    pub root: PathBuf,
    pub node_name: String,
    pub node_ip: String,
    pub recorder: EventRecorder,
    pub cache: RuntimeCache,
    pub workers: Mutex<HashMap<String, watch::Sender<Option<Arc<Pod>>>>>,
    pub pods: RwLock<Option<Store<Pod>>>,
    pub cluster_dns: Vec<String>,
    pub cluster_domain: String,
}

pub struct Kubelet {
    pub cfg: KubeletConfig,
    pub ctx: Arc<KubeletCtx>,
}

impl Kubelet {
    pub fn new(client: Client, runtime: Arc<dyn Runtime>, cfg: KubeletConfig) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&cfg.root_dir)?;
        let root = std::fs::canonicalize(&cfg.root_dir)?;
        let ctx = Arc::new(KubeletCtx {
            recorder: EventRecorder::new(client.clone(), "kubelet").with_host(&cfg.node_name),
            client,
            runtime,
            root,
            node_name: cfg.node_name.clone(),
            node_ip: cfg.node_ip.clone(),
            cache: RuntimeCache::default(),
            workers: Mutex::new(HashMap::new()),
            pods: RwLock::new(None),
            cluster_dns: cfg.cluster_dns.clone(),
            cluster_domain: cfg.cluster_domain.clone(),
        });
        Ok(Self { cfg, ctx })
    }

    /// Sends the latest pod object to its worker, starting the worker if needed.
    fn dispatch(&self, uid: &str, pod: Option<Arc<Pod>>) {
        let mut workers = self.ctx.workers.lock().unwrap();
        if let Some(tx) = workers.get(uid) {
            let _ = tx.send(pod);
            return;
        }
        let Some(p) = pod else { return };
        let (tx, rx) = watch::channel(Some(p));
        workers.insert(uid.to_string(), tx);
        let w = pod_workers::PodWorker::new(self.ctx.clone(), uid.to_string());
        tokio::spawn(w.run(rx));
    }

    /// Removes runtime objects and pod directories of pods this node no longer has
    /// (`HandlePodCleanups`).
    async fn housekeeping(&self, store: &Store<Pod>) {
        let known: HashSet<String> = store.list().iter().filter_map(|p| p.metadata.uid.clone()).collect();
        let working: HashSet<String> = self.ctx.workers.lock().unwrap().keys().cloned().collect();
        let (sandboxes, containers) = self.ctx.cache.all();
        let rt = &self.ctx.runtime;
        let mut orphans = HashSet::new();
        for c in &containers {
            if !known.contains(&c.pod_uid) && !working.contains(&c.pod_uid) {
                let _ = rt.remove_container(&c.id).await;
                orphans.insert(c.pod_uid.clone());
            }
        }
        for s in &sandboxes {
            if !known.contains(&s.pod_uid) && !working.contains(&s.pod_uid) {
                let _ = rt.stop_pod_sandbox(&s.id).await;
                let _ = rt.remove_pod_sandbox(&s.id).await;
                orphans.insert(s.pod_uid.clone());
            }
        }
        if let Ok(dirs) = std::fs::read_dir(self.ctx.root.join("pods")) {
            for d in dirs.flatten() {
                let uid = d.file_name().to_string_lossy().to_string();
                if !known.contains(&uid) && !working.contains(&uid) {
                    volumes::teardown(&self.ctx.root, &uid);
                }
            }
        }
        if !orphans.is_empty() {
            tracing::info!(count = orphans.len(), "cleaned up orphaned pods");
            self.ctx.cache.refresh(rt.as_ref()).await;
        }
    }

    /// Runs the kubelet until the process stops.
    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        let version = self.ctx.runtime.version().await?;
        tracing::info!(node = %self.cfg.node_name, runtime = %version, root = %self.ctx.root.display(), "starting kubelet");
        nodestatus::register(&self.ctx, &self.cfg, &version).await?;

        // Kubelet API server (logs, exec).
        let addr = format!("0.0.0.0:{}", self.cfg.port);
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        match &self.cfg.tls {
            Some(t) => {
                let tls = k8s_client_go::util::cert::server_config(&t.cert, &t.key, t.client_ca.as_deref()).map_err(anyhow::Error::msg)?;
                let srv = server::secure_router(self.ctx.clone(), t.clone());
                tracing::info!(webhook_authz = t.webhook_authz, x509 = t.client_ca.is_some(), "kubelet server listening on https://{addr}");
                tokio::spawn(async move {
                    let _ = k8s_apiserver::server::serve_router_tls(srv, listener, tls, std::future::pending()).await;
                });
            }
            None => {
                let srv = server::router(self.ctx.clone());
                tracing::warn!("kubelet server listening on http://{addr} without authentication (no --tls-cert-file)");
                tokio::spawn(async move {
                    let _ = axum::serve(listener, srv).await;
                });
            }
        }

        // Heartbeats: Lease every 10s, node status every 10s.
        let me = self.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_secs(10));
            loop {
                t.tick().await;
                if let Err(e) = nodestatus::heartbeat(&me.ctx, &me.cfg, &version).await {
                    tracing::warn!("node heartbeat failed: {e:#}");
                }
            }
        });

        // PLEG: relist the runtime every second.
        let ctx = self.ctx.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_secs(1));
            loop {
                t.tick().await;
                ctx.cache.refresh(ctx.runtime.as_ref()).await;
            }
        });

        // Pods bound to this node.
        let lp = ListParams::default().fields(&format!("spec.nodeName={}", self.cfg.node_name));
        let inf = Informer::new(Api::<Pod>::all(self.ctx.client.clone()), lp);
        let mut rx = inf.subscribe();
        inf.start();
        inf.wait_synced().await;
        *self.ctx.pods.write().unwrap() = Some(inf.store());
        self.ctx.cache.refresh(self.ctx.runtime.as_ref()).await;
        for p in inf.store().list() {
            self.dispatch(&p.metadata.uid.clone().unwrap_or_default(), Some(p));
        }
        let me = self.clone();
        let store = inf.store();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_secs(10));
            loop {
                t.tick().await;
                me.housekeeping(&store).await;
            }
        });
        loop {
            match rx.recv().await {
                Ok(Event::Added(p)) | Ok(Event::Updated { new: p, .. }) => {
                    self.dispatch(&p.metadata.uid.clone().unwrap_or_default(), Some(p))
                }
                Ok(Event::Deleted(p)) => self.dispatch(p.metadata.uid.as_deref().unwrap_or(""), None),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    for p in inf.store().list() {
                        self.dispatch(&p.metadata.uid.clone().unwrap_or_default(), Some(p));
                    }
                }
                Err(_) => return Ok(()),
            }
        }
    }
}
