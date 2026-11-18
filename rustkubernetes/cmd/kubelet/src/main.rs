//! `kubelet` (`cmd/kubelet/app/server.go`).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use k8s_api::core::v1::Taint;
use k8s_client_go::tools::clientcmd;
use k8s_client_go::{Client, Config};
use kube_kubelet::dockershim::DockerRuntime;
use kube_kubelet::{Kubelet, KubeletConfig};

#[derive(Parser, Debug)]
#[command(name = "kubelet", version, about = "The kubelet is the primary node agent that runs on each node.")]
struct Options {
    /// Path to a kubeconfig file for the API server.
    #[arg(long, env = "KUBECONFIG")]
    kubeconfig: Option<PathBuf>,
    /// API server address (overrides kubeconfig).
    #[arg(long)]
    master: Option<String>,
    /// Node name (defaults to the hostname).
    #[arg(long)]
    hostname_override: Option<String>,
    /// IP address of the node, used by the API server to reach the kubelet.
    #[arg(long, default_value = "127.0.0.1")]
    node_ip: String,
    /// Directory for pod volumes and kubelet state.
    #[arg(long)]
    root_dir: Option<PathBuf>,
    /// Port of the kubelet API (logs, exec).
    #[arg(long, default_value_t = 10250)]
    port: u16,
    /// Container runtime endpoint (Docker Engine API socket).
    #[arg(long, default_value = "unix:///var/run/docker.sock")]
    container_runtime_endpoint: String,
    /// Image of the pod sandbox ("pause") container.
    #[arg(long, default_value = "registry.k8s.io/pause:3.10")]
    pod_infra_container_image: String,
    #[arg(long, default_value_t = 110)]
    max_pods: u32,
    /// Labels added when registering the node, `key=value,...`.
    #[arg(long, value_delimiter = ',')]
    node_labels: Vec<String>,
    /// Taints added when registering the node, `key=value:Effect,...`.
    #[arg(long, value_delimiter = ',')]
    register_with_taints: Vec<String>,
    /// DNS server IPs for pods with `dnsPolicy: ClusterFirst`.
    #[arg(long, value_delimiter = ',')]
    cluster_dns: Vec<String>,
    /// Cluster domain appended to search paths.
    #[arg(long, default_value = "cluster.local")]
    cluster_domain: String,
    /// Serving certificate for the kubelet API (enables HTTPS + authentication).
    #[arg(long)]
    tls_cert_file: Option<PathBuf>,
    #[arg(long)]
    tls_private_key_file: Option<PathBuf>,
    /// CA that signs client certificates (x509 authentication of the API server).
    #[arg(long)]
    client_ca_file: Option<PathBuf>,
    /// Allow anonymous requests to the kubelet API.
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    anonymous_auth: bool,
    /// `Webhook` (SubjectAccessReview against the API server) or `AlwaysAllow`.
    #[arg(long, default_value = "Webhook")]
    authorization_mode: String,
    #[arg(short = 'v', long = "v", default_value_t = 1)]
    v: u8,
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_lowercase()).unwrap_or_else(|_| "localhost".into())
}

fn parse_taint(s: &str) -> anyhow::Result<Taint> {
    let (kv, effect) = s.rsplit_once(':').with_context(|| format!("invalid taint {s:?}, want key=value:Effect"))?;
    let (key, value) = kv.split_once('=').map(|(k, v)| (k, Some(v.to_string()))).unwrap_or((kv, None));
    Ok(Taint { key: key.into(), value, effect: effect.into(), time_added: None })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let o = Options::parse();
    let level = match o.v {
        0 | 1 => "info",
        2 | 3 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).init();

    let mut cfg = match (&o.kubeconfig, &o.master) {
        (Some(p), _) => clientcmd::load_config(p, None)?,
        (None, Some(m)) => Config::new(m),
        (None, None) => Config::infer().context("no --kubeconfig or --master given")?,
    };
    if let Some(m) = &o.master {
        cfg.server = m.trim_end_matches('/').to_string();
    }
    cfg.user_agent = format!("kubelet/{} (rustkubernetes)", env!("CARGO_PKG_VERSION"));
    let node_name = o.hostname_override.clone().unwrap_or_else(hostname);
    let root_dir = o.root_dir.clone().unwrap_or_else(|| {
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".local/share/rustkubernetes/kubelet").join(&node_name)
    });
    let mut labels = vec![];
    for kv in o.node_labels.iter().filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').with_context(|| format!("invalid label {kv:?}"))?;
        labels.push((k.to_string(), v.to_string()));
    }
    let taints = o.register_with_taints.iter().filter(|s| !s.is_empty()).map(|s| parse_taint(s)).collect::<anyhow::Result<Vec<_>>>()?;

    let tls = match (&o.tls_cert_file, &o.tls_private_key_file) {
        (Some(c), Some(k)) => Some(kube_kubelet::server::TlsServing {
            cert: std::fs::read(c).with_context(|| format!("reading {}", c.display()))?,
            key: std::fs::read(k).with_context(|| format!("reading {}", k.display()))?,
            client_ca: o.client_ca_file.as_ref().map(std::fs::read).transpose().context("reading --client-ca-file")?,
            anonymous: o.anonymous_auth,
            webhook_authz: match o.authorization_mode.as_str() {
                "Webhook" => true,
                "AlwaysAllow" => false,
                m => anyhow::bail!("unknown --authorization-mode {m:?} (want Webhook or AlwaysAllow)"),
            },
        }),
        (None, None) => None,
        _ => anyhow::bail!("--tls-cert-file and --tls-private-key-file must be given together"),
    };
    k8s_client_go::util::cert::install_crypto_provider();
    let runtime = Arc::new(DockerRuntime::new(&o.container_runtime_endpoint, &node_name, &o.pod_infra_container_image));
    let kubelet = Arc::new(Kubelet::new(
        Client::new(cfg)?,
        runtime,
        KubeletConfig {
            node_name,
            node_ip: o.node_ip,
            root_dir,
            port: o.port,
            max_pods: o.max_pods,
            node_labels: labels,
            register_taints: taints,
            cluster_dns: o.cluster_dns.iter().filter(|s| !s.is_empty()).cloned().collect(),
            cluster_domain: o.cluster_domain.clone(),
            tls,
        },
    )?);
    tokio::select! {
        r = kubelet.run() => r,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("kubelet stopping (containers keep running and are adopted on restart)");
            Ok(())
        }
    }
}
