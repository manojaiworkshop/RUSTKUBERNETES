//! Port of `pkg/controller/nodeipam/ipam/range_allocator.go`: gives every
//! node without `spec.podCIDRs` the next free subnet of each cluster CIDR
//! (one per IP family) and returns subnets of deleted nodes to the pool.

use std::sync::{Arc, Mutex};

use k8s_api::core::v1::{Node, ObjectReference};
use k8s_apimachinery::net::IpNet;
use k8s_client_go::tools::cache::{Event, Store};
use k8s_client_go::tools::record::{EventRecorder, NORMAL};
use k8s_client_go::util::workqueue::WorkQueue;
use k8s_client_go::{Api, Client, PatchType};
use serde_json::json;

use super::cidrset::CidrSet;
use super::CIDR_UPDATE_RETRIES;
use crate::controller_utils::*;

/// `CIDRAllocatorParams`
#[derive(Clone, Debug, Default)]
pub struct AllocatorParams {
    pub cluster_cidrs: Vec<IpNet>,
    pub service_cidr: Option<IpNet>,
    pub secondary_service_cidr: Option<IpNet>,
    pub node_cidr_mask_sizes: Vec<u8>,
}

pub struct RangeAllocator {
    client: Client,
    node_store: Store<Node>,
    cluster_cidrs: Vec<IpNet>,
    /// One per cluster CIDR, same index.
    cidr_sets: Mutex<Vec<CidrSet>>,
    recorder: EventRecorder,
    pub(crate) queue: Queue,
}

fn pod_cidrs(node: &Node) -> Vec<String> {
    node.spec.as_ref().and_then(|s| s.pod_cidrs.clone()).unwrap_or_default()
}

/// `controllerutil.RecordNodeStatusChange`
pub fn record_node_status_change(recorder: &EventRecorder, node: &Node, new_status: &str) {
    let name = node.metadata.name().to_string();
    tracing::debug!(status = new_status, node = %name, "recording status change event message for node");
    let r = ObjectReference {
        api_version: Some("v1".into()),
        kind: Some("Node".into()),
        name: Some(name.clone()),
        uid: node.metadata.uid.clone(),
        namespace: Some(String::new()),
        ..Default::default()
    };
    recorder.event_ref(r, NORMAL, new_status, format!("Node {name} status is now: {new_status}"));
}

impl RangeAllocator {
    /// `NewCIDRRangeAllocator`: builds the CIDR sets, removes the Service
    /// ranges from them and occupies the CIDRs of `existing` nodes.
    pub fn new(
        client: Client,
        node_store: Store<Node>,
        recorder: EventRecorder,
        params: &AllocatorParams,
        existing: &[Arc<Node>],
    ) -> anyhow::Result<Arc<Self>> {
        let mut sets = vec![];
        for (idx, cidr) in params.cluster_cidrs.iter().enumerate() {
            let size = *params.node_cidr_mask_sizes.get(idx).ok_or_else(|| anyhow::anyhow!("no node CIDR mask size for {cidr}"))?;
            sets.push(CidrSet::new(*cidr, size)?);
        }
        let ra = Arc::new(Self {
            client,
            node_store,
            cluster_cidrs: params.cluster_cidrs.clone(),
            cidr_sets: Mutex::new(sets),
            recorder,
            queue: WorkQueue::new(),
        });
        match &params.service_cidr {
            Some(s) => ra.filter_out_service_range(s),
            None => tracing::info!("No Service CIDR provided. Skipping filtering out service addresses"),
        }
        match &params.secondary_service_cidr {
            Some(s) => ra.filter_out_service_range(s),
            None => tracing::info!("No Secondary Service CIDR provided. Skipping filtering out secondary service addresses"),
        }
        for node in existing {
            if pod_cidrs(node).is_empty() {
                tracing::debug!(node = %node.metadata.name(), "Node has no CIDR, ignoring");
                continue;
            }
            tracing::debug!(node = %node.metadata.name(), "Node has CIDR, occupying it in CIDR map");
            // Garbage or out-of-range CIDRs mean the configuration changed: fail like upstream.
            ra.occupy_cidrs(node)?;
        }
        Ok(ra)
    }

    /// `filterOutServiceRange`: Service ClusterIPs must never become pod IPs.
    fn filter_out_service_range(&self, service: &IpNet) {
        let mut sets = self.cidr_sets.lock().unwrap();
        for (idx, cidr) in self.cluster_cidrs.iter().enumerate() {
            if !cidr.overlaps(service) {
                continue;
            }
            if let Err(e) = sets[idx].occupy(service) {
                tracing::error!(service_cidr = %service, "Error filtering out service cidr out cluster cidr: {e}");
            }
        }
    }

    /// Feeds node events into the queue; deletions release the node's CIDRs
    /// straight away (the deleted object is only available in the handler).
    pub fn handle(&self, ev: Event<Node>) {
        match ev {
            Event::Added(n) | Event::Updated { new: n, .. } => self.queue.add(n.metadata.name().to_string()),
            Event::Deleted(n) => {
                if let Err(e) = self.release_cidr(&n) {
                    tracing::error!(node = %n.metadata.name(), "Error while processing CIDR Release: {e}");
                }
                self.queue.add(n.metadata.name().to_string());
            }
        }
    }

    /// `syncNode`
    pub async fn sync_node(&self, key: &str) -> SyncResult {
        let Some(node) = self.node_store.get(key) else {
            tracing::debug!(node = key, "node has been deleted");
            return Ok(None);
        };
        if node.metadata.deletion_timestamp.is_some() {
            tracing::debug!(node = key, "node is being deleted");
            self.release_cidr(&node)?;
            return Ok(None);
        }
        self.allocate_or_occupy_cidr(&node).await?;
        Ok(None)
    }

    /// `occupyCIDRs`
    pub fn occupy_cidrs(&self, node: &Node) -> anyhow::Result<()> {
        let name = node.metadata.name();
        let mut sets = self.cidr_sets.lock().unwrap();
        for (idx, cidr) in pod_cidrs(node).iter().enumerate() {
            let pod_cidr: IpNet = cidr.parse().map_err(|_| anyhow::anyhow!("failed to parse node {name}, CIDR {cidr}"))?;
            let Some(set) = sets.get_mut(idx) else {
                anyhow::bail!(
                    "node:{name} has an allocated cidr: {cidr} at index:{idx} that does not exist in cluster cidrs configuration"
                );
            };
            set.occupy(&pod_cidr)
                .map_err(|e| anyhow::anyhow!("failed to mark cidr[{pod_cidr}] at idx [{idx}] as occupied for node: {name}: {e}"))?;
        }
        Ok(())
    }

    /// `ReleaseCIDR`
    pub fn release_cidr(&self, node: &Node) -> anyhow::Result<()> {
        let name = node.metadata.name();
        let mut sets = self.cidr_sets.lock().unwrap();
        for (idx, cidr) in pod_cidrs(node).iter().enumerate() {
            let pod_cidr: IpNet = cidr.parse().map_err(|e| anyhow::anyhow!("failed to parse CIDR {cidr} on Node {name}: {e}"))?;
            let Some(set) = sets.get_mut(idx) else {
                anyhow::bail!(
                    "node:{name} has an allocated cidr: {cidr} at index:{idx} that does not exist in cluster cidrs configuration"
                );
            };
            tracing::debug!(cidr = %cidr, node = name, "Release CIDR for node");
            set.release(&pod_cidr).map_err(|e| anyhow::anyhow!("error when releasing CIDR {cidr}: {e}"))?;
        }
        Ok(())
    }

    fn release_all(&self, cidrs: &[IpNet]) {
        let mut sets = self.cidr_sets.lock().unwrap();
        for (idx, c) in cidrs.iter().enumerate() {
            if let Err(e) = sets[idx].release(c) {
                tracing::error!(index = idx, cidr = %c, "Error when releasing CIDR: {e}");
            }
        }
    }

    /// `AllocateOrOccupyCIDR`
    pub async fn allocate_or_occupy_cidr(&self, node: &Node) -> anyhow::Result<()> {
        if !pod_cidrs(node).is_empty() {
            return self.occupy_cidrs(node);
        }
        let mut allocated = vec![];
        {
            let mut sets = self.cidr_sets.lock().unwrap();
            for idx in 0..sets.len() {
                match sets[idx].allocate_next() {
                    Ok(c) => allocated.push(c),
                    Err(e) => {
                        for (i, c) in allocated.iter().enumerate() {
                            let _ = sets[i].release(c);
                        }
                        drop(sets);
                        record_node_status_change(&self.recorder, node, "CIDRNotAvailable");
                        anyhow::bail!("failed to allocate cidr from cluster cidr at idx:{idx}: {e}");
                    }
                }
            }
        }
        tracing::debug!(node = %node.metadata.name(), cidrs = ?allocated.iter().map(ToString::to_string).collect::<Vec<_>>(), "Putting node with CIDR into the work queue");
        self.update_cidrs_allocation(node.metadata.name(), &allocated).await
    }

    /// `updateCIDRsAllocation`
    async fn update_cidrs_allocation(&self, node_name: &str, allocated: &[IpNet]) -> anyhow::Result<()> {
        let cidrs: Vec<String> = allocated.iter().map(ToString::to_string).collect();
        let Some(node) = self.node_store.get(node_name) else {
            self.release_all(allocated);
            anyhow::bail!("Failed while getting node {node_name} for updating Node.Spec.PodCIDRs");
        };
        let current = pod_cidrs(&node);
        // We possibly updated this node already and just failed to see the result.
        if current == cidrs {
            tracing::debug!(node = node_name, cidrs = ?cidrs, "Node already has allocated CIDR. It matches the proposed one");
            return Ok(());
        }
        if !current.is_empty() {
            tracing::error!(node = node_name, pod_cidrs = ?current, "Node already has a CIDR allocated. Releasing the new one");
            self.release_all(allocated);
            return Ok(());
        }
        let api: Api<Node> = Api::all(self.client.clone());
        let mut last = None;
        for _ in 0..CIDR_UPDATE_RETRIES {
            match patch_node_cidrs(&api, node_name, &cidrs).await {
                Ok(()) => {
                    tracing::info!(node = node_name, pod_cidrs = ?cidrs, "Set node PodCIDR");
                    return Ok(());
                }
                Err(e) => last = Some(e),
            }
        }
        let err = last.unwrap();
        tracing::error!(node = node_name, pod_cidrs = ?cidrs, "Failed to update node PodCIDR after multiple attempts: {err}");
        record_node_status_change(&self.recorder, &node, "CIDRAssignmentFailed");
        // A timeout may still have applied the patch: leak rather than hand out twice.
        if !err.to_string().contains("Timeout") {
            tracing::error!(node = node_name, "CIDR assignment for node failed. Releasing allocated CIDR");
            self.release_all(allocated);
        }
        Err(err)
    }
}

/// `nodeutil.PatchNodeCIDRs`: sets `podCIDRs` and the legacy `podCIDR`.
pub async fn patch_node_cidrs(api: &Api<Node>, node: &str, cidrs: &[String]) -> anyhow::Result<()> {
    let patch = json!({"spec": {"podCIDR": cidrs[0], "podCIDRs": cidrs}});
    api.patch(node, PatchType::Strategic, &patch).await.map_err(|e| anyhow::anyhow!("failed to patch node CIDR: {e}"))?;
    Ok(())
}
