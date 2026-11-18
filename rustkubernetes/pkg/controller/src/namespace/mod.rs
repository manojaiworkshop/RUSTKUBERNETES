//! Port of `pkg/controller/namespace` (`deletion/namespaced_resources_deleter.go`).
//!
//! For a namespace marked for deletion (phase `Terminating`): discover every
//! namespaced resource, `deletecollection` it, and requeue until nothing is
//! left; then remove the `kubernetes` finalizer via `namespaces/<n>/finalize`
//! so the API server deletes the namespace.

use std::sync::Arc;
use std::time::Duration;

use k8s_api::core::v1::Namespace;
use k8s_client_go::api::DynamicApi;
use k8s_client_go::discovery::server_preferred_resources;
use k8s_client_go::tools::cache::{Informer, Store};
use k8s_client_go::util::workqueue::WorkQueue;
use k8s_client_go::{Api, Client, DeleteParams, ListParams};
use serde_json::json;

use crate::controller_utils::*;
use crate::ControllerContext;

pub const FINALIZER: &str = "kubernetes";

pub struct Controller {
    client: Client,
    store: Store<Namespace>,
    inf: Arc<Informer<Namespace>>,
    queue: Queue,
    resync: Duration,
}

impl Controller {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let inf = ctx.informers.informer::<Namespace>();
        let c = Arc::new(Self {
            client: ctx.client.clone(),
            store: inf.store(),
            inf: inf.clone(),
            queue: WorkQueue::new(),
            resync: ctx.config.resync_period,
        });
        let me = c.clone();
        on_events(inf.subscribe(), move |ev| {
            if ev.object().metadata.deletion_timestamp.is_some() {
                me.queue.add(key(ev.object().as_ref()));
            }
        });
        c
    }

    pub async fn run(self: Arc<Self>, workers: usize) {
        self.inf.wait_synced().await;
        let store = self.store.clone();
        resync_every(self.queue.clone(), self.resync, move || {
            store.list().into_iter().filter(|n| n.metadata.deletion_timestamp.is_some()).map(|n| key(n.as_ref())).collect()
        });
        let me = self.clone();
        run_workers("namespace-controller", self.queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.sync(&k).await }
        })
        .await;
    }

    async fn sync(&self, name: &str) -> SyncResult {
        let Some(ns) = self.store.get(name) else { return Ok(None) };
        if ns.metadata.deletion_timestamp.is_none() {
            return Ok(None);
        }
        let finalizers = ns.spec.as_ref().and_then(|s| s.finalizers.clone()).unwrap_or_default();
        if !finalizers.iter().any(|f| f == FINALIZER) {
            return Ok(None);
        }
        let remaining = self.delete_all_content(name).await?;
        if remaining > 0 {
            tracing::debug!(namespace = name, remaining, "waiting for namespace content to be deleted");
            return Ok(Some(Duration::from_secs(2)));
        }
        // Remove our finalizer through the finalize subresource.
        let api: Api<Namespace> = Api::all(self.client.clone());
        let mut obj = serde_json::to_value(ns.as_ref())?;
        let kept: Vec<&String> = finalizers.iter().filter(|f| *f != FINALIZER).collect();
        obj["spec"]["finalizers"] = json!(kept);
        match api.dynamic().replace_sub(name, "finalize", &obj).await {
            Ok(_) => tracing::info!(namespace = name, "namespace content deleted, finalized"),
            Err(e) if e.is_not_found() => {}
            Err(e) => return Err(e.into()),
        }
        Ok(None)
    }

    /// Issues deletecollection for every namespaced resource; returns how many objects remain.
    async fn delete_all_content(&self, ns: &str) -> anyhow::Result<usize> {
        let resources = server_preferred_resources(&self.client).await?;
        let mut remaining = 0;
        for r in resources.iter().filter(|r| r.resource.namespaced && r.supports("list")) {
            let api = DynamicApi::new(self.client.clone(), r.resource.clone(), Some(ns));
            if r.supports("deletecollection") {
                if let Err(e) = api.delete_collection(&ListParams::default(), &DeleteParams::background()).await {
                    if !e.is_not_found() {
                        tracing::warn!(namespace = ns, resource = %r.resource.plural, "deletecollection failed: {e}");
                    }
                }
            }
            let left = api.list(&ListParams::default()).await?;
            remaining += left["items"].as_array().map(Vec::len).unwrap_or(0);
        }
        Ok(remaining)
    }
}
