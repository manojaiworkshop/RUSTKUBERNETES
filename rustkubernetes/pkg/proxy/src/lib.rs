//! Port of `k8s.io/kubernetes/pkg/proxy`: the Service proxy.
//!
//! | Module | Go equivalent |
//! |--------|---------------|
//! | [`serviceport`] | `serviceport.go`, `servicechangetracker.go` |
//! | [`endpoints`] | `endpoint.go`, `endpointslicecache.go`, `topology.go` |
//! | [`nftables`] | `pkg/proxy/nftables/proxier.go` |
//! | [`runner`] | `pkg/util/async/bounded_frequency_runner.go` |
//! | [`conntrack`] | `pkg/proxy/conntrack` |
//! | [`hairpin`] | `pkg/kubelet/network/hairpin` (kubelet's job upstream) |
//! | [`healthcheck`] | `pkg/proxy/healthcheck`, `pkg/proxy/metrics` |
//!
//! [`Proxier::run`] watches Services and EndpointSlices; every change triggers a
//! (rate-limited) full sync that renders the nftables ruleset and loads it if it
//! differs from what was last applied (or the table disappeared).

pub mod conntrack;
pub mod endpoints;
pub mod hairpin;
pub mod healthcheck;
pub mod nftables;
pub mod runner;
pub mod serviceport;

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use k8s_api::core::v1::Service;
use k8s_api::discovery::v1::EndpointSlice;
use k8s_client_go::tools::cache::{Informer, Store};
use k8s_client_go::{Api, Client, ListParams};

use endpoints::{categorize, endpoints_map, EndpointsMap};
use healthcheck::ProxierHealth;
use nftables::{Nft, NftConfig};
use runner::BoundedFrequencyRunner;
use serviceport::{service_ports, Protocol, ServicePortInfo, ServicePortName};

#[derive(Clone, Debug)]
pub struct ProxyConfig {
    pub nft: NftConfig,
    pub min_sync_period: Duration,
    pub sync_period: Duration,
    /// Bridge whose ports get hairpin mode (e.g. `docker0`); `None` leaves them alone.
    pub hairpin_bridge: Option<String>,
}

/// What a UDP service port looked like at the last sync (for conntrack cleanup).
#[derive(Clone, Debug, PartialEq)]
struct UdpState {
    info: ServicePortInfo,
    endpoints: BTreeSet<Ipv4Addr>,
}

pub struct Proxier {
    pub cfg: ProxyConfig,
    nft: Arc<dyn Nft>,
    conntrack: conntrack::Conntrack,
    pub health: Arc<ProxierHealth>,
    last_applied: Mutex<Option<String>>,
    udp: Mutex<BTreeMap<ServicePortName, UdpState>>,
}

impl Proxier {
    pub fn new(cfg: ProxyConfig, nft: Arc<dyn Nft>, conntrack: conntrack::Conntrack) -> Arc<Self> {
        let health = ProxierHealth::new(cfg.sync_period * 2);
        Arc::new(Self { cfg, nft, conntrack, health, last_applied: Mutex::new(None), udp: Mutex::new(BTreeMap::new()) })
    }

    /// Service ports and endpoints from the informer caches.
    fn snapshot(services: &Store<Service>, slices: &Store<EndpointSlice>) -> (BTreeMap<ServicePortName, ServicePortInfo>, EndpointsMap) {
        let mut ports = BTreeMap::new();
        for svc in services.list() {
            for p in service_ports(&svc) {
                ports.insert(p.name.clone(), p);
            }
        }
        let slices = slices.list();
        (ports, endpoints_map(slices.iter().map(|s| s.as_ref())))
    }

    /// `syncProxyRules`.
    pub async fn sync(&self, services: &Store<Service>, slices: &Store<EndpointSlice>) {
        let start = Instant::now();
        let (ports, eps) = Self::snapshot(services, slices);
        let script = nftables::ruleset(&self.cfg.nft, &ports, &eps);
        let changed = self.last_applied.lock().unwrap().as_deref() != Some(script.as_str());
        let need = changed
            || match self.nft.table_exists(&self.cfg.nft.table).await {
                Ok(exists) => !exists, // flushed by someone else: restore
                Err(e) => {
                    tracing::warn!("checking nftables table: {e:#}");
                    true
                }
            };
        let endpoint_count: usize = eps.values().map(Vec::len).sum();
        if need {
            match self.nft.apply(&script).await {
                Ok(()) => {
                    *self.last_applied.lock().unwrap() = Some(script.clone());
                    tracing::info!(services = ports.len(), endpoints = endpoint_count, took = ?start.elapsed(), "synced nftables rules");
                }
                Err(e) => {
                    tracing::error!("failed to sync nftables rules: {e:#}");
                    self.health.synced(false, start.elapsed(), ports.len(), endpoint_count, script.len());
                    return;
                }
            }
        }
        self.cleanup_udp(&ports, &eps).await;
        if let Some(br) = &self.cfg.hairpin_bridge {
            hairpin::ensure(br, "bridge").await;
        }
        self.health.synced(true, start.elapsed(), ports.len(), endpoint_count, script.len());
    }

    /// Deletes conntrack entries that would keep UDP clients on dead paths.
    async fn cleanup_udp(&self, ports: &BTreeMap<ServicePortName, ServicePortInfo>, eps: &EndpointsMap) {
        let node = &self.cfg.nft.node_name;
        let now: BTreeMap<ServicePortName, UdpState> = ports
            .iter()
            .filter(|(k, _)| k.protocol == Protocol::Udp)
            .map(|(k, info)| {
                let c = categorize(info, eps.get(k).map(Vec::as_slice).unwrap_or(&[]), node);
                (k.clone(), UdpState { info: info.clone(), endpoints: c.cluster.iter().map(|e| e.ip).collect() })
            })
            .collect();
        let before = std::mem::replace(&mut *self.udp.lock().unwrap(), now.clone());
        for (k, cur) in &now {
            let prev = before.get(k);
            let ext: Vec<Ipv4Addr> = cur.info.external_ips.iter().chain(cur.info.load_balancer_ips.iter()).copied().collect();
            // Service went from no endpoints to some: flush cached rejects.
            if !cur.endpoints.is_empty() && prev.is_none_or(|p| p.endpoints.is_empty()) {
                self.conntrack.clear_service(cur.info.cluster_ip, cur.info.port).await;
                for ip in &ext {
                    self.conntrack.clear_service(*ip, cur.info.port).await;
                }
                if let Some(np) = cur.info.node_port {
                    self.conntrack.clear_node_port(np).await;
                }
            }
            // Endpoints removed: flows NATed to them are dead.
            if let Some(p) = prev {
                for gone in p.endpoints.difference(&cur.endpoints) {
                    self.conntrack.clear_nat(cur.info.cluster_ip, *gone).await;
                    for ip in &ext {
                        self.conntrack.clear_nat(*ip, *gone).await;
                    }
                }
            }
        }
    }

    /// Runs until the process stops.
    pub async fn run(self: Arc<Self>, client: Client) -> anyhow::Result<()> {
        let svc_inf = Informer::new(Api::<Service>::all(client.clone()), ListParams::default());
        // Slices of services handled by other proxies are filtered by their Service.
        let slice_inf = Informer::new(Api::<EndpointSlice>::all(client), ListParams::default());
        let mut svc_rx = svc_inf.subscribe();
        let mut slice_rx = slice_inf.subscribe();
        svc_inf.start();
        slice_inf.start();
        svc_inf.wait_synced().await;
        slice_inf.wait_synced().await;
        tracing::info!("caches synced, starting nftables proxier");

        let runner = BoundedFrequencyRunner::new(self.cfg.min_sync_period, self.cfg.sync_period);
        let trigger = runner.trigger();
        let health = self.health.clone();
        tokio::spawn(async move {
            loop {
                let ok = tokio::select! {
                    r = svc_rx.recv() => !matches!(r, Err(tokio::sync::broadcast::error::RecvError::Closed)),
                    r = slice_rx.recv() => !matches!(r, Err(tokio::sync::broadcast::error::RecvError::Closed)),
                };
                if !ok {
                    break;
                }
                health.queued();
                trigger.notify_one();
            }
        });
        let (services, slices) = (svc_inf.store(), slice_inf.store());
        runner.run(|| self.sync(&services, &slices)).await;
        Ok(())
    }
}
