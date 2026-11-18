//! `cmd/kube-controller-manager/app/options`: command-line flags.

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "kube-controller-manager",
    version,
    about = "The Kubernetes controller manager is a daemon that embeds the core control loops shipped with Kubernetes."
)]
pub struct Options {
    /// Path to a kubeconfig file with authorization and master location information.
    #[arg(long, env = "KUBECONFIG")]
    pub kubeconfig: Option<PathBuf>,

    /// The address of the Kubernetes API server (overrides any value in kubeconfig).
    #[arg(long)]
    pub master: Option<String>,

    /// A list of controllers to enable. '*' enables all on-by-default controllers,
    /// 'foo' enables the controller named 'foo', '-foo' disables the controller named 'foo'.
    #[arg(long, value_delimiter = ',', default_value = "*")]
    pub controllers: Vec<String>,

    /// Start a leader election client and gain leadership before executing the main loop.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub leader_elect: bool,

    /// Duration non-leader candidates wait before trying to acquire leadership (seconds).
    #[arg(long, default_value_t = 15)]
    pub leader_elect_lease_duration: u64,

    /// Interval between attempts by the acting leader to renew leadership (seconds).
    #[arg(long, default_value_t = 10)]
    pub leader_elect_renew_deadline: u64,

    /// Wait between tries of actions (seconds).
    #[arg(long, default_value_t = 2)]
    pub leader_elect_retry_period: u64,

    /// Number of workers per controller (`--concurrent-*-syncs`).
    #[arg(long, default_value_t = 5)]
    pub concurrent_syncs: usize,

    /// Period after which a node without heartbeats is marked Unknown (seconds).
    #[arg(long, default_value_t = 40)]
    pub node_monitor_grace_period: u64,

    /// Period for syncing node status (seconds).
    #[arg(long, default_value_t = 5)]
    pub node_monitor_period: u64,

    /// Number of terminated pods that can exist before the pod GC deletes them (0 disables).
    #[arg(long, default_value_t = 12500)]
    pub terminated_pod_gc_threshold: usize,

    /// Cluster CA published to every namespace as the `kube-root-ca.crt` ConfigMap.
    #[arg(long)]
    pub root_ca_file: Option<std::path::PathBuf>,

    /// Re-enqueue every object this often (seconds).
    #[arg(long, default_value_t = 30)]
    pub min_resync_period: u64,

    /// Address for the health/metrics server.
    #[arg(long, default_value = "127.0.0.1")]
    pub bind_address: String,

    /// Port for the health server (`/healthz`).
    #[arg(long, alias = "secure-port", default_value_t = 10257)]
    pub port: u16,

    /// Log level verbosity (0-1 info, 2-3 debug, 4+ trace). `RUST_LOG` overrides.
    #[arg(short = 'v', long = "v", default_value_t = 1)]
    pub v: u8,
}
