//! `cmd/kube-controller-manager/app` (`controllermanager.go`):
//! build the client, serve `/healthz`, win leader election, then start every
//! enabled controller sharing one informer factory.

pub mod options;

use std::time::Duration;

use anyhow::Context;
use k8s_client_go::tools::clientcmd;
use k8s_client_go::tools::leaderelection::{LeaderElectionConfig, LeaderElector};
use k8s_client_go::{Client, Config};
use kube_controller::nodeipam::ipam::range_allocator::AllocatorParams;
use kube_controller::nodeipam::ipam::{CLOUD_ALLOCATOR_TYPE, RANGE_ALLOCATOR_TYPE};
use kube_controller::nodeipam::NodeIpamConfig;
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

/// `startNodeIpamController` checks: parses `--cluster-cidr` and the Service
/// ranges and picks the node mask size per IP family (`setNodeCIDRMaskSizes`).
pub fn node_ipam_config(opts: &Options) -> anyhow::Result<NodeIpamConfig> {
    use k8s_apimachinery::net::{is_dual_stack, parse_cidrs};
    let mut cfg = NodeIpamConfig {
        allocate_node_cidrs: opts.allocate_node_cidrs,
        allocator_type: opts.cidr_allocator_type.clone(),
        ..Default::default()
    };
    if !opts.allocate_node_cidrs {
        return Ok(cfg);
    }
    if cfg.allocator_type != RANGE_ALLOCATOR_TYPE && cfg.allocator_type != CLOUD_ALLOCATOR_TYPE {
        anyhow::bail!("invalid --cidr-allocator-type {:?} (want RangeAllocator or CloudAllocator)", cfg.allocator_type);
    }
    let clusters = parse_cidrs(&opts.cluster_cidr).map_err(|e| anyhow::anyhow!("failed to parse --cluster-cidr: {e}"))?;
    if clusters.is_empty() {
        anyhow::bail!("Controller: Must specify --cluster-cidr if --allocate-node-cidrs is set");
    }
    if clusters.len() > 2 {
        anyhow::bail!("len of clusters cidrs is:{} > more than max allowed of 2", clusters.len());
    }
    let dual = is_dual_stack(&clusters);
    if clusters.len() > 1 && !dual {
        anyhow::bail!("cluster cidrs must be dual stack (one IPv4 and one IPv6) when more than one is given");
    }
    let sizes: Vec<u8> = if dual {
        if opts.node_cidr_mask_size != 0 {
            anyhow::bail!("usage of --node-cidr-mask-size is not allowed with dual-stack clusters");
        }
        clusters.iter().map(|c| if c.is_ipv4() { opts.node_cidr_mask_size_ipv4 } else { opts.node_cidr_mask_size_ipv6 }).collect()
    } else {
        let c = &clusters[0];
        let size = match (opts.node_cidr_mask_size, c.is_ipv4()) {
            (0, true) => opts.node_cidr_mask_size_ipv4,
            (0, false) => opts.node_cidr_mask_size_ipv6,
            (n, _) => n,
        };
        vec![size]
    };
    for (c, size) in clusters.iter().zip(&sizes) {
        if c.prefix > *size || *size > c.bits() {
            anyhow::bail!("Controller: Invalid --cluster-cidr, mask size of cluster CIDR must be less than or equal to --node-cidr-mask-size configured for CIDR family");
        }
    }
    let parse_one = |flag: &str, v: &str| -> anyhow::Result<Option<k8s_apimachinery::net::IpNet>> {
        if v.trim().is_empty() {
            return Ok(None);
        }
        v.trim().parse().map(Some).map_err(|e| anyhow::anyhow!("failed to parse --{flag}: {e}"))
    };
    cfg.params = AllocatorParams {
        cluster_cidrs: clusters,
        service_cidr: parse_one("service-cluster-ip-range", &opts.service_cluster_ip_range)?,
        secondary_service_cidr: parse_one("secondary-service-cluster-ip-range", &opts.secondary_service_cluster_ip_range)?,
        node_cidr_mask_sizes: sizes,
    };
    Ok(cfg)
}

/// `CSRSigningControllerOptions` → config, with its `Validate`.
fn csr_signing_config(o: &Options) -> anyhow::Result<kube_controller::certificates::signer::CsrSigningConfig> {
    let c = kube_controller::certificates::signer::CsrSigningConfig {
        cluster_signing_cert_file: o.cluster_signing_cert_file.clone(),
        cluster_signing_key_file: o.cluster_signing_key_file.clone(),
        kubelet_serving: (o.cluster_signing_kubelet_serving_cert_file.clone(), o.cluster_signing_kubelet_serving_key_file.clone()),
        kubelet_client: (o.cluster_signing_kubelet_client_cert_file.clone(), o.cluster_signing_kubelet_client_key_file.clone()),
        kube_apiserver_client: (
            o.cluster_signing_kube_apiserver_client_cert_file.clone(),
            o.cluster_signing_kube_apiserver_client_key_file.clone(),
        ),
        legacy_unknown: (o.cluster_signing_legacy_unknown_cert_file.clone(), o.cluster_signing_legacy_unknown_key_file.clone()),
        cluster_signing_duration: o.cluster_signing_duration,
    };
    c.validate().map_err(anyhow::Error::msg)?;
    Ok(c)
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
        node_ipam: node_ipam_config(&opts)?,
        volume: kube_controller::VolumeConfig {
            enable_hostpath_provisioner: opts.enable_hostpath_provisioner,
            pv_claim_binder_sync_period: Duration::from_secs(opts.pvclaimbinder_sync_period.max(1)),
        },
        csr_signing: csr_signing_config(&opts)?,
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
