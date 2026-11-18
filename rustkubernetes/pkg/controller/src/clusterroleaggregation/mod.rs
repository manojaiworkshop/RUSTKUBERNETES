//! Port of `pkg/controller/clusterroleaggregation`: fills the rules of
//! ClusterRoles that have an `aggregationRule` (`admin`, `edit`, `view`, ...) with
//! the rules of every ClusterRole matching its label selectors, so adding a
//! labelled role (`rbac.authorization.k8s.io/aggregate-to-edit: "true"`) extends
//! the default roles.

use std::sync::Arc;

use k8s_api::rbac::v1::{ClusterRole, PolicyRule};
use k8s_apimachinery::labels::Selector;
use k8s_client_go::informers::SharedInformerFactory;
use k8s_client_go::tools::cache::Store;
use k8s_client_go::util::workqueue::WorkQueue;
use k8s_client_go::{Api, Client};

use crate::controller_utils::*;
use crate::ControllerContext;

pub struct Controller {
    client: Client,
    store: Store<ClusterRole>,
    informers: Arc<SharedInformerFactory>,
    queue: Queue,
}

impl Controller {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let inf = ctx.informers.informer::<ClusterRole>();
        let c =
            Arc::new(Self { client: ctx.client.clone(), store: inf.store(), informers: ctx.informers.clone(), queue: WorkQueue::new() });
        // Any change may affect any aggregated role: enqueue all of them.
        let me = c.clone();
        on_events(inf.subscribe(), move |_| {
            for cr in me.store.list() {
                if cr.aggregation_rule.is_some() {
                    me.queue.add(cr.metadata.name().to_string());
                }
            }
        });
        c
    }

    pub async fn run(self: Arc<Self>, workers: usize) {
        self.informers.wait_for_cache_sync().await;
        let me = self.clone();
        run_workers("clusterrole-aggregation-controller", self.queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.sync(&k).await }
        })
        .await;
    }

    async fn sync(&self, name: &str) -> SyncResult {
        let Some(cr) = self.store.get(name) else { return Ok(None) };
        let Some(selectors) = cr.aggregation_rule.as_ref().and_then(|a| a.cluster_role_selectors.clone()) else { return Ok(None) };
        let selectors: Vec<Selector> = selectors.iter().filter_map(|s| Selector::from_label_selector(s).ok()).collect();
        let mut sources: Vec<Arc<ClusterRole>> = self
            .store
            .list()
            .into_iter()
            .filter(|c| c.metadata.name() != name)
            .filter(|c| {
                let labels = c.metadata.labels.clone().unwrap_or_default();
                selectors.iter().any(|s| s.matches(&labels))
            })
            .collect();
        sources.sort_by(|a, b| a.metadata.name().cmp(b.metadata.name()));
        let mut rules: Vec<PolicyRule> = vec![];
        for s in sources {
            for r in s.rules.iter().flatten() {
                if !rules.contains(r) {
                    rules.push(r.clone());
                }
            }
        }
        if cr.rules.as_ref() == Some(&rules) {
            return Ok(None);
        }
        let mut updated = (*cr).clone();
        updated.rules = Some(rules);
        Api::<ClusterRole>::all(self.client.clone()).replace(&updated).await?;
        tracing::info!(clusterrole = name, "updated aggregated rules");
        Ok(None)
    }
}
