//! Port of `pkg/controller/serviceaccount` (`serviceaccounts_controller.go`):
//! ensures the `default` ServiceAccount exists in every active namespace.

use std::sync::Arc;
use std::time::Duration;

use k8s_api::core::v1::{Namespace, ServiceAccount};
use k8s_apimachinery::meta::ObjectMeta;
use k8s_client_go::informers::SharedInformerFactory;
use k8s_client_go::tools::cache::{Event, Store};
use k8s_client_go::util::workqueue::WorkQueue;
use k8s_client_go::{Api, Client};

use crate::controller_utils::*;
use crate::ControllerContext;

pub const DEFAULT_SERVICE_ACCOUNT: &str = "default";

pub struct Controller {
    client: Client,
    ns_store: Store<Namespace>,
    sa_store: Store<ServiceAccount>,
    informers: Arc<SharedInformerFactory>,
    queue: Queue,
    resync: Duration,
}

impl Controller {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let ns_inf = ctx.informers.informer::<Namespace>();
        let sa_inf = ctx.informers.informer::<ServiceAccount>();
        let c = Arc::new(Self {
            client: ctx.client.clone(),
            ns_store: ns_inf.store(),
            sa_store: sa_inf.store(),
            informers: ctx.informers.clone(),
            queue: WorkQueue::new(),
            resync: ctx.config.resync_period,
        });
        let me = c.clone();
        on_events(ns_inf.subscribe(), move |ev| me.queue.add(ev.object().metadata.name().to_string()));
        let me = c.clone();
        on_events(sa_inf.subscribe(), move |ev| {
            if let Event::Deleted(sa) = ev {
                me.queue.add(sa.metadata.namespace().to_string());
            }
        });
        c
    }

    pub async fn run(self: Arc<Self>, workers: usize) {
        self.informers.wait_for_cache_sync().await;
        let store = self.ns_store.clone();
        resync_every(self.queue.clone(), self.resync, move || store.keys());
        let me = self.clone();
        run_workers("serviceaccount-controller", self.queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.sync(&k).await }
        })
        .await;
    }

    async fn sync(&self, ns: &str) -> SyncResult {
        let Some(n) = self.ns_store.get(ns) else { return Ok(None) };
        let active = n.metadata.deletion_timestamp.is_none() && n.status.as_ref().and_then(|s| s.phase.as_deref()) != Some("Terminating");
        if !active || self.sa_store.get_ns(ns, DEFAULT_SERVICE_ACCOUNT).is_some() {
            return Ok(None);
        }
        let api: Api<ServiceAccount> = Api::namespaced(self.client.clone(), ns);
        match api.create(&ServiceAccount::new(ObjectMeta::namespaced(ns, DEFAULT_SERVICE_ACCOUNT))).await {
            Ok(_) => Ok(None),
            Err(e) if e.is_already_exists() => Ok(None),
            // The namespace is being terminated: nothing to do.
            Err(e) if e.to_string().contains("being terminated") || e.is_not_found() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
