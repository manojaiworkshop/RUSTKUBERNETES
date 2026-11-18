//! `kube-proxy` (`cmd/kube-proxy/app/server.go`): programs nftables so that
//! Service ClusterIPs, NodePorts, externalIPs and load-balancer IPs reach their
//! endpoints.
//!
//! Needs `CAP_NET_ADMIN` in the node's network namespace (as root, or in a
//! container with `--network host --cap-add NET_ADMIN`, which is how
//! `hack/local-up-cluster.sh` runs it). `--dry-run` prints the rules instead.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use k8s_client_go::tools::clientcmd;
use k8s_client_go::{Client, Config};
use kube_proxy_core::conntrack::Conntrack;
use kube_proxy_core::nftables::{Nft, NftCommand, NftConfig, NftDryRun, DEFAULT_TABLE};
use kube_proxy_core::{Proxier, ProxyConfig};

#[derive(Parser, Debug)]
#[command(name = "kube-proxy", version, about = "The Kubernetes network proxy runs on each node (nftables mode).")]
struct Options {
    /// Path to a kubeconfig file with authorization and master location information.
    #[arg(long, env = "KUBECONFIG")]
    kubeconfig: Option<PathBuf>,
    /// The address of the Kubernetes API server (overrides any value in kubeconfig).
    #[arg(long)]
    master: Option<String>,
    /// Name of this node (endpoints on it are "local" for traffic policies).
    #[arg(long)]
    hostname_override: Option<String>,
    /// Which proxy mode to use; only `nftables` is implemented.
    #[arg(long, default_value = "nftables")]
    proxy_mode: String,
    /// Pod CIDR range(s); traffic to ClusterIPs from outside it is masqueraded.
    #[arg(long)]
    cluster_cidr: Option<String>,
    /// SNAT all traffic sent via Service cluster IPs.
    #[arg(long)]
    masquerade_all: bool,
    /// CIDRs of node addresses that accept NodePort traffic (default: all non-loopback local addresses).
    #[arg(long, value_delimiter = ',')]
    nodeport_addresses: Vec<String>,
    /// Maximum interval between full rule syncs.
    #[arg(long, default_value = "30s", value_parser = parse_duration)]
    sync_period: Duration,
    /// Minimum interval between rule syncs (changes are batched).
    #[arg(long, default_value = "1s", value_parser = parse_duration)]
    min_sync_period: Duration,
    /// ip:port for the health server (`/healthz`, `/livez`).
    #[arg(long, default_value = "0.0.0.0:10256")]
    healthz_bind_address: String,
    /// ip:port for `/metrics`.
    #[arg(long, default_value = "127.0.0.1:10249")]
    metrics_bind_address: String,
    /// nftables table to own.
    #[arg(long, default_value = DEFAULT_TABLE)]
    nft_table: String,
    /// Path of the `nft` binary.
    #[arg(long, default_value = "nft")]
    nft_path: String,
    /// Path of the `conntrack` binary (stale UDP flow cleanup).
    #[arg(long, default_value = "conntrack")]
    conntrack_path: String,
    /// Bridge whose ports get hairpin mode so pods can reach themselves through a
    /// Service (e.g. `docker0`). Needs the `bridge` tool (iproute2). Off by default.
    #[arg(long)]
    hairpin_bridge: Option<String>,
    /// Remove the proxy's rules and exit.
    #[arg(long)]
    cleanup: bool,
    /// Print the generated nftables scripts instead of applying them (no privileges needed).
    #[arg(long)]
    dry_run: bool,
    /// Log level verbosity (0-1 info, 2-3 debug, 4+ trace). `RUST_LOG` overrides.
    #[arg(short = 'v', long = "v", default_value_t = 1)]
    v: u8,
}

/// `30s`, `1m`, `500ms`, `2h`, or plain seconds.
fn parse_duration(s: &str) -> Result<Duration, String> {
    let (num, unit) = s.find(|c: char| !c.is_ascii_digit() && c != '.').map(|i| s.split_at(i)).unwrap_or((s, "s"));
    let n: f64 = num.parse().map_err(|_| format!("invalid duration {s:?}"))?;
    let secs = match unit {
        "ms" => n / 1000.0,
        "s" => n,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        _ => return Err(format!("invalid duration unit in {s:?}")),
    };
    Ok(Duration::from_secs_f64(secs))
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_lowercase()).unwrap_or_else(|_| "localhost".into())
}

fn init_logging(v: u8) {
    let level = match v {
        0 | 1 => "info",
        2 | 3 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    // Logs go to stderr so `--dry-run` output on stdout stays clean.
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).with_writer(std::io::stderr).init();
}

/// Serves health/metrics; like upstream, a failure to bind is logged and not fatal.
async fn serve(addr: &str, app: axum::Router) -> anyhow::Result<()> {
    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
        }
        Err(e) => tracing::error!("cannot serve on {addr}: {e} (continuing without it)"),
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let o = Options::parse();
    init_logging(o.v);
    if o.proxy_mode != "nftables" {
        anyhow::bail!("unsupported --proxy-mode {:?}: this kube-proxy implements nftables only", o.proxy_mode);
    }
    let nft: Arc<dyn Nft> = if o.dry_run { Arc::new(NftDryRun) } else { Arc::new(NftCommand { path: o.nft_path.clone() }) };
    if o.cleanup {
        nft.delete_table(&o.nft_table).await?;
        tracing::info!(table = %o.nft_table, "removed kube-proxy rules");
        return Ok(());
    }
    if !o.dry_run {
        // Fail early with a useful hint instead of on the first sync.
        nft.table_exists(&o.nft_table).await.map_err(|e| {
            anyhow::anyhow!("{e:#}\nkube-proxy needs CAP_NET_ADMIN in the node network namespace: run it as root, in a container with --network host --cap-add NET_ADMIN (see hack/local-up-cluster.sh), or use --dry-run")
        })?;
        if std::fs::read_to_string("/proc/sys/net/bridge/bridge-nf-call-iptables").map(|s| s.trim() != "1").unwrap_or(true) {
            tracing::warn!(
                "br_netfilter is not active (net.bridge.bridge-nf-call-iptables != 1): pod-to-service traffic on the same bridge may fail"
            );
        }
    }

    let mut cfg = match (&o.kubeconfig, &o.master) {
        (Some(p), _) => clientcmd::load_config(p, None)?,
        (None, Some(m)) => Config::new(m),
        (None, None) => Config::infer().context("no --kubeconfig or --master given")?,
    };
    if let Some(m) = &o.master {
        cfg.server = m.trim_end_matches('/').to_string();
    }
    cfg.user_agent = format!("kube-proxy/{} (rustkubernetes)", env!("CARGO_PKG_VERSION"));
    let node_name = o.hostname_override.clone().unwrap_or_else(hostname);
    let proxy_cfg = ProxyConfig {
        nft: NftConfig {
            table: o.nft_table.clone(),
            node_name: node_name.clone(),
            cluster_cidr: o.cluster_cidr.clone().filter(|s| !s.is_empty()),
            masquerade_all: o.masquerade_all,
            nodeport_addresses: o.nodeport_addresses.iter().filter(|s| !s.is_empty()).cloned().collect(),
        },
        min_sync_period: o.min_sync_period,
        sync_period: o.sync_period,
        hairpin_bridge: if o.dry_run { None } else { o.hairpin_bridge.clone().filter(|s| !s.is_empty()) },
    };
    tracing::info!(node = %node_name, mode = "nftables", table = %o.nft_table, cluster_cidr = ?proxy_cfg.nft.cluster_cidr, dry_run = o.dry_run, "starting kube-proxy (rustkubernetes)");
    let conntrack = if o.dry_run { Conntrack::detect("/nonexistent") } else { Conntrack::detect(&o.conntrack_path) };
    let proxier = Proxier::new(proxy_cfg, nft, conntrack);
    let health = proxier.health.router();
    serve(&o.healthz_bind_address, health.clone()).await?;
    if o.metrics_bind_address != o.healthz_bind_address {
        serve(&o.metrics_bind_address, health).await?;
    }
    tracing::info!("serving healthz on http://{}/healthz, metrics on http://{}/metrics", o.healthz_bind_address, o.metrics_bind_address);

    let client = Client::new(cfg)?;
    tokio::select! {
        r = proxier.run(client) => r,
        _ = tokio::signal::ctrl_c() => {
            // Like upstream, rules stay in place so Service traffic keeps flowing across restarts.
            tracing::info!("kube-proxy stopping (rules kept; run `kube-proxy --cleanup` to remove them)");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("1m").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("5").unwrap(), Duration::from_secs(5));
        assert!(parse_duration("5x").is_err());
    }
}
