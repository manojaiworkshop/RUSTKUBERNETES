//! `cmd/kube-controller-manager/app` (`controllermanager.go`):
//! build the client, serve `/healthz`, win leader election, then start every
//! enabled controller sharing one informer factory.

pub mod options;

use std::time::Duration;

use anyhow::Context;
use k8s_client_go::tools::clientcmd;
use k8s_client_go::tools::leaderelection::{LeaderElectionConfig, LeaderElector};
use k8s_client_go::{Client, Config};
use kube_controller::{controller_descriptors, is_enabled, ControllerConfig, ControllerContext};

use options::Options;

pub fn init_logging(v: u8) {
    let level = match v {
        0 | 1 => "info",
        2 | 3 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).init();
}

fn client_config(opts: &Options) -> anyhow::Result<Config> {
    let mut cfg = match &opts.kubeconfig {
        Some(p) => clientcmd::load_config(p, None)?,
        None => match &opts.master {
            Some(m) => Config::new(m),
            None => Config::infer().context("no --kubeconfig or --master given")?,
        },
    };
    if let Some(m) = &opts.master {
        cfg.server = m.trim_end_matches('/').to_string();
    }
    cfg.user_agent = format!("kube-controller-manager/{} (rustkubernetes)", env!("CARGO_PKG_VERSION"));
    Ok(cfg)
}

/// Starts every enabled controller and runs until they all stop.
pub async fn start_controllers(ctx: ControllerContext, flags: &[String]) {
    let mut runs = vec![];
    for d in controller_descriptors() {
        if is_enabled(&d, flags) {
            tracing::info!(controller = d.name, "starting controller");
            runs.push((d.constructor)(&ctx));
        } else {
            tracing::info!(controller = d.name, "controller is disabled");
        }
    }
    // Controllers have subscribed to their informers; start the shared watches.
    ctx.informers.start();
    ctx.informers.wait_for_cache_sync().await;
    tracing::info!("caches populated");
    futures::future::join_all(runs).await;
}

pub async fn run(opts: Options) -> anyhow::Result<()> {
    tracing::info!("starting kube-controller-manager (rustkubernetes)");
    let client = Client::new(client_config(&opts)?)?;

    // Health endpoint.
    let addr = format!("{}:{}", opts.bind_address, opts.port);
    let listener = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("binding {addr}"))?;
    tracing::info!("serving healthz on http://{addr}/healthz");
    tokio::spawn(async move {
        let app = axum::Router::new().route("/healthz", axum::routing::get(|| async { "ok" }));
        let _ = axum::serve(listener, app).await;
    });

    let config = ControllerConfig {
        workers: opts.concurrent_syncs,
        resync_period: Duration::from_secs(opts.min_resync_period),
        node_monitor_grace_period: Duration::from_secs(opts.node_monitor_grace_period),
        node_monitor_period: Duration::from_secs(opts.node_monitor_period),
        terminated_pod_gc_threshold: opts.terminated_pod_gc_threshold,
        root_ca: opts.root_ca_file.as_ref().map(std::fs::read_to_string).transpose().context("reading --root-ca-file")?,
    };
    let ctx = ControllerContext::new(client.clone(), config);
    let flags = opts.controllers.clone();

    if !opts.leader_elect {
        start_controllers(ctx, &flags).await;
        return Ok(());
    }
    let host =
        std::env::var("HOSTNAME").ok().or_else(|| std::fs::read_to_string("/etc/hostname").ok()).unwrap_or_else(|| "localhost".into());
    let identity = format!("{}_{}", host.trim(), uuid::Uuid::new_v4());
    let mut le = LeaderElectionConfig::new("kube-system", "kube-controller-manager", &identity);
    le.lease_duration = Duration::from_secs(opts.leader_elect_lease_duration);
    le.renew_deadline = Duration::from_secs(opts.leader_elect_renew_deadline);
    le.retry_period = Duration::from_secs(opts.leader_elect_retry_period);
    let elector = LeaderElector::new(client, le);
    let result = tokio::select! {
        r = elector.run(async move { start_controllers(ctx, &flags).await }) => r,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
            Ok(())
        }
    };
    if let Err(e) = result {
        // Like Go's OnStoppedLeading: exit so a fresh process can rejoin cleanly.
        anyhow::bail!("leaderelection lost: {e}");
    }
    Ok(())
}
