//! Port of `pkg/controller/nodeipam` (`node-ipam-controller`): assigns each
//! node a pod CIDR (`spec.podCIDR`, `spec.podCIDRs`) out of `--cluster-cidr`,
//! sized by `--node-cidr-mask-size`, never overlapping
//! `--service-cluster-ip-range`. Runs only with `--allocate-node-cidrs`.
//!
//! The kubelet hands the node's pod CIDR to the container runtime's network
//! plugin, and the pod network add-on routes each node's CIDR to that node.

pub mod ipam;

use std::sync::Arc;

use k8s_api::core::v1::Node;
use k8s_client_go::informers::SharedInformerFactory;

use crate::controller_utils::*;
use crate::ControllerContext;
use ipam::range_allocator::{AllocatorParams, RangeAllocator};

/// Node IPAM settings (`NodeIPAMControllerConfiguration` + `KubeCloudShared`).
#[derive(Clone, Debug, Default)]
pub struct NodeIpamConfig {
    /// `--allocate-node-cidrs`
    pub allocate_node_cidrs: bool,
    /// `--cidr-allocator-type`
    pub allocator_type: String,
    pub params: AllocatorParams,
}

pub struct Controller {
    ctx: ControllerContext,
    informers: Arc<SharedInformerFactory>,
}

impl Controller {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        // Subscribe now so the shared node informer is started with the others.
        let _ = ctx.informers.informer::<Node>();
        Arc::new(Self { ctx: ctx.clone(), informers: ctx.informers.clone() })
    }

    /// `startNodeIpamController` + `Controller.Run`.
    pub async fn run(self: Arc<Self>, workers: usize) {
        let cfg = &self.ctx.config.node_ipam;
        if !cfg.allocate_node_cidrs {
            tracing::info!("node-ipam-controller is disabled because --allocate-node-cidrs is false");
            return;
        }
        if cfg.allocator_type != ipam::RANGE_ALLOCATOR_TYPE {
            tracing::error!(allocator = %cfg.allocator_type, "unsupported CIDR allocator type (only RangeAllocator is implemented)");
            return;
        }
        let node_inf = self.informers.informer::<Node>();
        let events = node_inf.subscribe();
        self.informers.wait_for_cache_sync().await;
        let existing = node_inf.store().list();
        let ra = match RangeAllocator::new(
            self.ctx.client.clone(),
            node_inf.store(),
            self.ctx.recorder("cidrAllocator"),
            &cfg.params,
            &existing,
        ) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Failed to start node-ipam-controller: {e:#}");
                std::process::exit(1);
            }
        };
        tracing::info!(
            cluster_cidrs = ?cfg.params.cluster_cidrs.iter().map(ToString::to_string).collect::<Vec<_>>(),
            node_cidr_mask_sizes = ?cfg.params.node_cidr_mask_sizes,
            "Starting range CIDR allocator"
        );
        let h = ra.clone();
        on_events(events, move |ev| h.handle(ev));
        for n in &existing {
            ra.queue.add(n.metadata.name().to_string());
        }
        let me = ra.clone();
        run_workers("node-ipam-controller", ra.queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.sync_node(&k).await }
        })
        .await;
    }
}
