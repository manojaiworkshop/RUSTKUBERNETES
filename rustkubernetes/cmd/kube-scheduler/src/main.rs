//! `kube-scheduler` (`cmd/kube-scheduler/app/server.go`): loads the
//! `KubeSchedulerConfiguration`, serves `/healthz`, wins leader election and
//! runs the scheduling loop.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use k8s_client_go::tools::clientcmd;
use k8s_client_go::tools::leaderelection::{LeaderElectionConfig, LeaderElector};
use k8s_client_go::{Client, Config};
use kube_scheduler_core::apis::load_profiles;
use kube_scheduler_core::Scheduler;

#[derive(Parser, Debug)]
#[command(name = "kube-scheduler", version, about = "The Kubernetes scheduler is a control plane process which assigns Pods to Nodes.")]
struct Options {
    /// Path to a kubeconfig file with authorization and master location information.
    #[arg(long, env = "KUBECONFIG")]
    kubeconfig: Option<PathBuf>,
    /// The address of the Kubernetes API server (overrides any value in kubeconfig).
    #[arg(long)]
    master: Option<String>,
    /// The path to a KubeSchedulerConfiguration file (profiles, plugins, pluginConfig).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Start a leader election client and gain leadership before scheduling.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    leader_elect: bool,
    /// Address for the health server.
    #[arg(long, default_value = "127.0.0.1")]
    bind_address: String,
    /// Port for the health server (`/healthz`).
    #[arg(long, alias = "secure-port", default_value_t = 10259)]
    port: u16,
    /// Log level verbosity (0-1 info, 2-3 debug, 4+ trace). `RUST_LOG` overrides.
    #[arg(short = 'v', long = "v", default_value_t = 1)]
    v: u8,
}

fn init_logging(v: u8) {
    let level = match v {
        0 | 1 => "info",
        2 | 3 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts = Options::parse();
    init_logging(opts.v);
    tracing::info!("starting kube-scheduler (rustkubernetes)");

    let mut cfg = match (&opts.kubeconfig, &opts.master) {
        (Some(p), _) => clientcmd::load_config(p, None)?,
        (None, Some(m)) => Config::new(m),
        (None, None) => Config::infer().context("no --kubeconfig or --master given")?,
    };
    if let Some(m) = &opts.master {
        cfg.server = m.trim_end_matches('/').to_string();
    }
    cfg.user_agent = format!("kube-scheduler/{} (rustkubernetes)", env!("CARGO_PKG_VERSION"));
    let client = Client::new(cfg)?;

    let yaml = match &opts.config {
        Some(p) => Some(std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?),
        None => None,
    };
    let profiles = load_profiles(yaml.as_deref()).map_err(|e| anyhow::anyhow!(e))?;
    for p in &profiles {
        let filters: Vec<&str> = p.plugins.iter().map(|x| x.name()).collect();
        let scores: Vec<String> = p.scorers.iter().map(|(x, w)| format!("{}={w}", x.name())).collect();
        tracing::info!(profile = %p.scheduler_name, ?filters, ?scores, "scheduling profile");
    }

    let addr = format!("{}:{}", opts.bind_address, opts.port);
    let listener = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("binding {addr}"))?;
    tokio::spawn(async move {
        let app = axum::Router::new().route("/healthz", axum::routing::get(|| async { "ok" }));
        let _ = axum::serve(listener, app).await;
    });
    tracing::info!("serving healthz on http://{addr}/healthz");

    let sched = Scheduler::new(client.clone(), profiles);
    if !opts.leader_elect {
        sched.run().await;
        return Ok(());
    }
    let host =
        std::env::var("HOSTNAME").ok().or_else(|| std::fs::read_to_string("/etc/hostname").ok()).unwrap_or_else(|| "localhost".into());
    let identity = format!("{}_{}", host.trim(), uuid::Uuid::new_v4());
    let mut le = LeaderElectionConfig::new("kube-system", "kube-scheduler", &identity);
    le.lease_duration = Duration::from_secs(15);
    let elector = LeaderElector::new(client, le);
    tokio::select! {
        r = elector.run(async move { sched.run().await }) => {
            if let Err(e) = r {
                anyhow::bail!("leaderelection lost: {e}");
            }
        }
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }
    Ok(())
}
