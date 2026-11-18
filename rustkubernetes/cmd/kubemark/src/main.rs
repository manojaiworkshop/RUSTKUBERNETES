//! `kubemark` (`cmd/kubemark`): runs hollow nodes against a cluster.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use k8s_client_go::tools::clientcmd;
use k8s_client_go::{Client, Config};
use kube_kubemark::{HollowConfig, HollowNodes};

#[derive(Parser, Debug)]
#[command(name = "kubemark", version, about = "Hollow nodes: fake kubelets for testing the control plane without running containers.")]
struct Options {
    #[arg(long, env = "KUBECONFIG")]
    kubeconfig: Option<PathBuf>,
    #[arg(long)]
    master: Option<String>,
    /// Node names are `<prefix>-0 … <prefix>-(N-1)`.
    #[arg(long, default_value = "hollow-node")]
    name_prefix: String,
    /// Number of hollow nodes.
    #[arg(long, default_value_t = 3)]
    nodes: usize,
    /// CPU capacity per node.
    #[arg(long, default_value = "4")]
    cpu: String,
    /// Memory capacity per node.
    #[arg(long, default_value = "8Gi")]
    memory: String,
    /// Max pods per node.
    #[arg(long, default_value_t = 110)]
    max_pods: u32,
    /// Spread nodes over N zones (`topology.kubernetes.io/zone`).
    #[arg(long, default_value_t = 1)]
    zones: usize,
    /// Extra node labels, `key=value,key2=value2`.
    #[arg(long, value_delimiter = ',')]
    node_labels: Vec<String>,
    /// Delay before a bound pod is reported Running/Ready (milliseconds).
    #[arg(long, default_value_t = 500)]
    pod_startup_ms: u64,
    /// How long run-to-completion pods (restartPolicy Never/OnFailure) "run" (seconds).
    #[arg(long, default_value_t = 5)]
    job_duration_secs: u64,
    #[arg(short = 'v', long = "v", default_value_t = 1)]
    v: u8,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let o = Options::parse();
    let level = if o.v >= 2 { "debug" } else { "info" };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).init();

    let cfg = match (&o.kubeconfig, &o.master) {
        (_, Some(m)) => Config::new(m),
        (Some(p), None) => clientcmd::load_config(p, None)?,
        (None, None) => Config::infer().context("no --kubeconfig or --master given")?,
    };
    let mut labels = BTreeMap::new();
    for kv in o.node_labels.iter().filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').with_context(|| format!("invalid label {kv:?}, want key=value"))?;
        labels.insert(k.to_string(), v.to_string());
    }
    let hollow = HollowNodes::new(
        Client::new(cfg)?,
        HollowConfig {
            name_prefix: o.name_prefix,
            count: o.nodes,
            cpu: o.cpu,
            memory: o.memory,
            max_pods: o.max_pods,
            zones: o.zones,
            extra_labels: labels,
            startup_delay: Duration::from_millis(o.pod_startup_ms),
            job_duration: Duration::from_secs(o.job_duration_secs),
            ..Default::default()
        },
    );
    tracing::info!(nodes = ?hollow.node_names(), "starting hollow nodes");
    tokio::select! {
        r = hollow.run() => r,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("stopping hollow nodes (they will become NotReady after the grace period)");
            Ok(())
        }
    }
}
