//! Port of `pkg/controller/podgc`. Every 20 seconds:
//!
//! 1. deletes the oldest terminated (Succeeded/Failed) pods beyond
//!    `--terminated-pod-gc-threshold`;
//! 2. force-deletes pods bound to nodes that no longer exist (orphaned);
//! 3. force-deletes unscheduled pods that are terminating.

use std::sync::Arc;
use std::time::Duration;

use k8s_api::core::v1::{Node, Pod};
use k8s_client_go::informers::SharedInformerFactory;
use k8s_client_go::tools::cache::Store;
use k8s_client_go::{Api, Client, DeleteParams};

use crate::controller_utils::*;
use crate::ControllerContext;

const GC_PERIOD: Duration = Duration::from_secs(20);

pub struct Controller {
    client: Client,
    pod_store: Store<Pod>,
    node_store: Store<Node>,
    informers: Arc<SharedInformerFactory>,
    threshold: usize,
}

impl Controller {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let pod_inf = ctx.informers.informer::<Pod>();
        let node_inf = ctx.informers.informer::<Node>();
        Arc::new(Self {
            client: ctx.client.clone(),
            pod_store: pod_inf.store(),
            node_store: node_inf.store(),
            informers: ctx.informers.clone(),
            threshold: ctx.config.terminated_pod_gc_threshold,
        })
    }

    pub async fn run(self: Arc<Self>, _workers: usize) {
        self.informers.wait_for_cache_sync().await;
        let mut t = tokio::time::interval(GC_PERIOD);
        loop {
            t.tick().await;
            if let Err(e) = self.gc().await {
                tracing::warn!("pod gc failed: {e:#}");
            }
        }
    }

    async fn force_delete(&self, p: &Pod, why: &str) -> anyhow::Result<()> {
        let api: Api<Pod> = Api::namespaced(self.client.clone(), p.metadata.namespace());
        let dp = DeleteParams { grace_period_seconds: Some(0), precondition_uid: p.metadata.uid.clone(), ..Default::default() };
        match api.delete_with(p.metadata.name(), &dp).await {
            Ok(_) => {
                tracing::info!(pod = %format!("{}/{}", p.metadata.namespace(), p.metadata.name()), reason = why, "garbage collected pod")
            }
            Err(e) if e.is_not_found() || e.is_conflict() => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    pub async fn gc(&self) -> anyhow::Result<()> {
        let pods = self.pod_store.list();
        // 1. Terminated pods over the threshold, oldest first.
        let mut terminated: Vec<&Arc<Pod>> = pods.iter().filter(|p| matches!(pod_phase(p), "Succeeded" | "Failed")).collect();
        if self.threshold > 0 && terminated.len() > self.threshold {
            terminated.sort_by(|a, b| a.metadata.creation_timestamp.cmp(&b.metadata.creation_timestamp));
            let excess = terminated.len() - self.threshold;
            for p in terminated.into_iter().take(excess) {
                self.force_delete(p, "terminated pod threshold exceeded").await?;
            }
        }
        let node_api: Api<Node> = Api::all(self.client.clone());
        for p in &pods {
            let node = p.spec.as_ref().and_then(|s| s.node_name.clone()).filter(|n| !n.is_empty());
            match node {
                // 2. Orphaned: the node is gone (double-check with the API server).
                Some(n) if self.node_store.get(&n).is_none() => {
                    if node_api.get_opt(&n).await?.is_none() {
                        self.force_delete(p, "node no longer exists").await?;
                    }
                }
                // 3. Unscheduled and terminating.
                None if p.metadata.deletion_timestamp.is_some() => self.force_delete(p, "unscheduled terminating pod").await?,
                _ => {}
            }
        }
        Ok(())
    }
}
