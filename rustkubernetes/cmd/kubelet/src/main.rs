//! `kubelet` (`cmd/kubelet/app/server.go`).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use k8s_api::core::v1::Taint;
use k8s_client_go::tools::clientcmd;
use k8s_client_go::util::cert::DynamicCert;
use k8s_client_go::{Client, Config};
use kube_kubelet::certificate;
use kube_kubelet::dockershim::{network, DockerRuntime};
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
    /// The directory where the TLS certs are located. If --tls-cert-file and
    /// --tls-private-key-file are provided, this flag will be ignored. (default <root-dir>/pki)
    #[arg(long)]
    cert_dir: Option<PathBuf>,
    /// Path to a kubeconfig file that will be used to get client certificate for kubelet. If the
    /// file specified by --kubeconfig does not exist, the bootstrap kubeconfig is used to request
    /// a client certificate from the API server. On success, a kubeconfig file referencing the
    /// generated client certificate and key is written to the path specified by --kubeconfig.
    #[arg(long)]
    bootstrap_kubeconfig: Option<PathBuf>,
    /// Enables client certificate rotation. The Kubelet will request a new certificate from the
    /// certificates.k8s.io API when the current one approaches expiration.
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set, num_args = 0..=1, default_missing_value = "true")]
    rotate_certificates: bool,
    /// Auto-request and rotate the kubelet serving certificates by requesting new certificates from
    /// the kube-apiserver when the certificate expiration approaches (serverTLSBootstrap).
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set, num_args = 0..=1, default_missing_value = "true")]
    rotate_server_certificates: bool,
    /// CA that signs client certificates (x509 authentication of the API server).
    #[arg(long)]
    client_ca_file: Option<PathBuf>,
    /// Allow anonymous requests to the kubelet API.
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    anonymous_auth: bool,
    /// `Webhook` (SubjectAccessReview against the API server) or `AlwaysAllow`.
    #[arg(long, default_value = "Webhook")]
    authorization_mode: String,
    /// The name of the network plugin: "" (Docker's default bridge) or "kubenet"
    /// (bridge on the node's pod CIDR).
    #[arg(long, default_value = "")]
    network_plugin: String,
    /// MTU of the kubenet bridge; 0 uses the smallest MTU of the host's interfaces.
    #[arg(long, default_value_t = 0)]
    network_plugin_mtu: i32,
    /// Name of the kubenet bridge and its Docker network (rustkubernetes extension, upstream: always cbr0).
    #[arg(long, default_value = "cbr0")]
    kubenet_bridge_name: String,
    /// The CIDR to use for pod IP addresses, only used in standalone mode. In cluster mode,
    /// this is obtained from the control plane (`spec.podCIDR`).
    #[arg(long, default_value = "")]
    pod_cidr: String,
    #[arg(short = 'v', long = "v", default_value_t = 1)]
    v: u8,
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_lowercase()).unwrap_or_else(|_| "localhost".into())
}

/// The static `rustkube-portforward` helper installed next to the kubelet.
fn portforward_helper() -> Option<PathBuf> {
    let p = std::env::current_exe().ok()?.parent()?.join("rustkube-portforward");
    if p.is_file() {
        Some(p)
    } else {
        tracing::warn!(path = %p.display(), "rustkube-portforward not found: kubectl port-forward will fail on this node");
        None
    }
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

    let node_name = o.hostname_override.clone().unwrap_or_else(hostname);
    let root_dir = o.root_dir.clone().unwrap_or_else(|| {
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".local/share/rustkubernetes/kubelet").join(&node_name)
    });
    let cert_dir = o.cert_dir.clone().unwrap_or_else(|| root_dir.join("pki"));
    k8s_client_go::util::cert::install_crypto_provider();
    // TLS bootstrapping (`bootstrap.LoadClientCert`).
    if let Some(boot) = &o.bootstrap_kubeconfig {
        let kc = o.kubeconfig.as_ref().context("--bootstrap-kubeconfig needs --kubeconfig (where the new kubeconfig is written)")?;
        kube_kubelet::certificate::load_client_cert(kc, boot, &cert_dir, &node_name)
            .await
            .context("failed to run Kubelet: unable to load bootstrap kubeconfig")?;
    }
    let mut cfg = match (&o.kubeconfig, &o.master) {
        (Some(p), _) => clientcmd::load_config(p, None)?,
        (None, Some(m)) => Config::new(m),
        (None, None) => Config::infer().context("no --kubeconfig or --master given")?,
    };
    if let Some(m) = &o.master {
        cfg.server = m.trim_end_matches('/').to_string();
    }
    cfg.user_agent = format!("kubelet/{} (rustkubernetes)", env!("CARGO_PKG_VERSION"));
    let mut labels = vec![];
    for kv in o.node_labels.iter().filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').with_context(|| format!("invalid label {kv:?}"))?;
        labels.push((k.to_string(), v.to_string()));
    }
    kube_kubelet::apis::validate_node_labels(&labels.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>()).map_err(anyhow::Error::msg)?;
    let taints = o.register_with_taints.iter().filter(|s| !s.is_empty()).map(|s| parse_taint(s)).collect::<anyhow::Result<Vec<_>>>()?;

    // Client certificate rotation (`buildClientCertificateManager`): the client's
    // certificate comes from the manager, which renews it through that same client.
    let mut client_manager = None;
    if o.rotate_certificates && cfg.server.starts_with("https://") {
        let mut store = certificate::FileStore::new(&cert_dir, "kubelet-client");
        store.fallback = cfg.tls.cert_data.clone().zip(cfg.tls.key_data.clone());
        let cert = DynamicCert::new();
        cfg.tls.cert_resolver = Some(cert.clone());
        let client = Client::new(cfg.clone())?;
        client_manager = Some(certificate::Manager::new(
            "kubelet-client",
            client,
            store,
            certificate::Template::kubelet_client(&node_name),
            cert,
            true,
        )?);
    }
    let client = Client::new(cfg)?;

    let read = |p: &PathBuf| std::fs::read(p).with_context(|| format!("reading {}", p.display()));
    let serving_cert = DynamicCert::new();
    let mut server_manager = None;
    let serving = if o.rotate_server_certificates {
        // serverTLSBootstrap: request the serving certificate; --tls-cert-file is only a fallback.
        let mut store = certificate::FileStore::new(&cert_dir, "kubelet-server");
        if let (Some(c), Some(k)) = (&o.tls_cert_file, &o.tls_private_key_file) {
            store.fallback = Some((read(c)?, read(k)?));
        }
        let ips: Vec<std::net::IpAddr> = o.node_ip.parse().into_iter().collect();
        let t = certificate::Template::kubelet_serving(&node_name, std::slice::from_ref(&node_name), &ips);
        server_manager = Some(certificate::Manager::new("kubelet-serving", client.clone(), store, t, serving_cert.clone(), false)?);
        true
    } else {
        match (&o.tls_cert_file, &o.tls_private_key_file) {
            (Some(c), Some(k)) => {
                serving_cert.set(&read(c)?, &read(k)?).map_err(anyhow::Error::msg)?;
                true
            }
            (None, None) => false,
            _ => anyhow::bail!("--tls-cert-file and --tls-private-key-file must be given together"),
        }
    };
    let tls = if serving {
        Some(kube_kubelet::server::TlsServing {
            cert: serving_cert,
            client_ca: o.client_ca_file.as_ref().map(std::fs::read).transpose().context("reading --client-ca-file")?,
            anonymous: o.anonymous_auth,
            webhook_authz: match o.authorization_mode.as_str() {
                "Webhook" => true,
                "AlwaysAllow" => false,
                m => anyhow::bail!("unknown --authorization-mode {m:?} (want Webhook or AlwaysAllow)"),
            },
        })
    } else {
        None
    };
    for m in [client_manager, server_manager].into_iter().flatten() {
        tokio::spawn(m.run());
    }
    let plugin = network::Plugin::new(
        &network::NetworkConfig { plugin: o.network_plugin.clone(), mtu: o.network_plugin_mtu, bridge: o.kubenet_bridge_name.clone() },
        &node_name,
    )?;
    let runtime = Arc::new(
        DockerRuntime::new(&o.container_runtime_endpoint, &node_name, &o.pod_infra_container_image)
            .with_network(plugin, root_dir.join("dockershim"))
            .with_portforward_helper(portforward_helper()),
    );
    let kubelet = Arc::new(Kubelet::new(
        client,
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
            pod_cidr: Some(o.pod_cidr.clone()).filter(|s| !s.is_empty()),
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
