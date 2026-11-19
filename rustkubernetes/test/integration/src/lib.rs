//! Integration test framework (`test/integration/framework`).
//!
//! [`start_test_server`] boots a complete kube-apiserver (in-memory storage)
//! on a random local port and returns a client connected to it — the
//! equivalent of Go's `kubeapiservertesting.StartTestServer`.

use std::net::SocketAddr;
use std::sync::Arc;

use k8s_apiserver::{ApiServer, MemoryStore};
use k8s_client_go::{Client, Config};
use kube_controlplane::{bootstrap, new_server};

pub struct TestServer {
    pub server: Arc<ApiServer>,
    pub client: Client,
    pub addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

pub async fn start_test_server() -> TestServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = kube_controlplane::Config { advertise_port: addr.port(), ..Default::default() };
    let server = new_server(&cfg, MemoryStore::new()).unwrap();
    bootstrap(&server, &cfg).await.unwrap();
    k8s_apiextensions_apiserver::start(server.clone()).await;
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let srv = server.clone();
    tokio::spawn(async move {
        srv.serve_listener(listener, async move {
            let _ = rx.await;
        })
        .await
        .unwrap();
    });
    let client = Client::new(Config::new(format!("http://{addr}"))).unwrap();
    TestServer { server, client, addr, shutdown: Some(tx) }
}

// ---------------------------------------------------------------------------
// Controller test helpers
// ---------------------------------------------------------------------------

use std::future::Future;
use std::time::Duration;

use k8s_api::core::v1::{Pod, PodCondition};
use k8s_client_go::{Api, ListParams};
use kube_controller::{controller_descriptors, is_enabled, ControllerConfig, ControllerContext};

/// Fast settings for tests.
pub fn test_controller_config() -> ControllerConfig {
    ControllerConfig {
        workers: 2,
        resync_period: Duration::from_secs(1),
        node_monitor_grace_period: Duration::from_secs(2),
        node_monitor_period: Duration::from_millis(500),
        terminated_pod_gc_threshold: 100,
        root_ca: None,
        ..Default::default()
    }
}

/// Starts the named controllers (same names as `--controllers`) in the background.
pub fn start_controllers(client: &Client, names: &[&str]) {
    let ctx = ControllerContext::new(client.clone(), test_controller_config());
    let flags: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    let mut runs = vec![];
    for d in controller_descriptors() {
        if is_enabled(&d, &flags) {
            runs.push((d.constructor)(&ctx));
        }
    }
    ctx.informers.start();
    for r in runs {
        tokio::spawn(r);
    }
}

/// A stand-in for the kubelet (until Phase 4): every pod becomes Running and
/// Ready with an IP; pods marked for deletion are removed.
pub fn simulate_kubelet(client: &Client) {
    let pods: Api<Pod> = Api::all(client.clone());
    tokio::spawn(async move {
        let mut n: u32 = 0;
        loop {
            if let Ok(list) = pods.list(&ListParams::default()).await {
                for p in list.items {
                    let ns = p.metadata.namespace().to_string();
                    let api: Api<Pod> = Api::namespaced(pods.dynamic_client(), &ns);
                    if p.metadata.deletion_timestamp.is_some() {
                        let _ = api.dynamic().delete(p.metadata.name(), Some(0)).await;
                        continue;
                    }
                    let st = p.status.clone().unwrap_or_default();
                    if st.phase.as_deref() != Some("Pending") || p.metadata.labels().contains_key("test/manual") {
                        continue;
                    }
                    n += 1;
                    let mut p = p;
                    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                    let s = p.status.get_or_insert_with(Default::default);
                    s.phase = Some("Running".into());
                    s.pod_ip = Some(format!("10.244.{}.{}", n / 250, n % 250 + 1));
                    s.conditions = Some(vec![PodCondition {
                        type_: "Ready".into(),
                        status: "True".into(),
                        last_transition_time: Some(now),
                        ..Default::default()
                    }]);
                    let _ = api.replace_status(&p).await;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
}

/// Polls `f` until it returns `Some` or the timeout expires.
pub async fn eventually<T, F, Fut>(what: &str, timeout: Duration, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(v) = f().await {
            return v;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for: {what}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---------------------------------------------------------------------------
// Scheduler + hollow node helpers
// ---------------------------------------------------------------------------

use kube_kubemark::{HollowConfig, HollowNodes};
use kube_scheduler_core::apis::Profile;
use kube_scheduler_core::Scheduler;

/// Starts the scheduler (default profile) in the background.
pub fn start_scheduler(client: &Client) -> Arc<Scheduler> {
    let s = Scheduler::new(client.clone(), vec![Profile::default_profile()]);
    tokio::spawn(s.clone().run());
    s
}

/// Registers hollow nodes and runs them in the background; returns their names.
pub async fn start_hollow_nodes(client: &Client, prefix: &str, count: usize, cpu: &str, labels: &[(&str, &str)]) -> Vec<String> {
    let cfg = HollowConfig {
        name_prefix: prefix.into(),
        count,
        cpu: cpu.into(),
        extra_labels: labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        startup_delay: Duration::from_millis(50),
        job_duration: Duration::from_millis(300),
        ..Default::default()
    };
    let h = HollowNodes::new(client.clone(), cfg);
    h.register().await.unwrap();
    let names = h.node_names();
    tokio::spawn(h.run());
    names
}
