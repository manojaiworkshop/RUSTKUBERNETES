//! Port of `pkg/controller/certificates/rootcacertpublisher`: publishes the
//! cluster CA as the `kube-root-ca.crt` ConfigMap in every namespace, so pods can
//! verify the API server (it is part of the projected `kube-api-access-*` volume).
//! Runs only with `--root-ca-file`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use k8s_api::core::v1::{ConfigMap, Namespace};
use k8s_apimachinery::meta::ObjectMeta;
use k8s_client_go::informers::SharedInformerFactory;
use k8s_client_go::tools::cache::Store;
use k8s_client_go::util::workqueue::WorkQueue;
use k8s_client_go::{Api, Client};

use crate::controller_utils::*;
use crate::ControllerContext;

pub const CONFIGMAP_NAME: &str = "kube-root-ca.crt";
pub const DESCRIPTION: &str = "Contains a CA bundle that can be used to verify the kube-apiserver when using internal endpoints such as the internal service IP or kubernetes.default.svc. No other usage is guaranteed across distributions of Kubernetes clusters.";

pub struct Controller {
    client: Client,
    ca: Option<String>,
    ns_store: Store<Namespace>,
    cm_store: Store<ConfigMap>,
    informers: Arc<SharedInformerFactory>,
    queue: Queue,
    resync: Duration,
}

impl Controller {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let ns_inf = ctx.informers.informer::<Namespace>();
        let cm_inf = ctx.informers.informer::<ConfigMap>();
        let c = Arc::new(Self {
            client: ctx.client.clone(),
            ca: ctx.config.root_ca.clone(),
            ns_store: ns_inf.store(),
            cm_store: cm_inf.store(),
            informers: ctx.informers.clone(),
            queue: WorkQueue::new(),
            resync: ctx.config.resync_period,
        });
        let me = c.clone();
        on_events(ns_inf.subscribe(), move |ev| me.queue.add(ev.object().metadata.name().to_string()));
        let me = c.clone();
        on_events(cm_inf.subscribe(), move |ev| {
            let cm = ev.object();
            if cm.metadata.name() == CONFIGMAP_NAME {
                me.queue.add(cm.metadata.namespace().to_string());
            }
        });
        c
    }

    pub async fn run(self: Arc<Self>, workers: usize) {
        if self.ca.is_none() {
            tracing::info!("root-ca-cert-publisher: no --root-ca-file, not publishing kube-root-ca.crt");
            return;
        }
        self.informers.wait_for_cache_sync().await;
        let store = self.ns_store.clone();
        resync_every(self.queue.clone(), self.resync, move || store.keys());
        let me = self.clone();
        run_workers("root-ca-certificate-publisher-controller", self.queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.sync(&k).await }
        })
        .await;
    }

    async fn sync(&self, ns: &str) -> SyncResult {
        let (Some(n), Some(ca)) = (self.ns_store.get(ns), self.ca.as_ref()) else { return Ok(None) };
        if n.metadata.deletion_timestamp.is_some() {
            return Ok(None);
        }
        let data: BTreeMap<String, String> = [("ca.crt".to_string(), ca.clone())].into_iter().collect();
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), ns);
        match self.cm_store.get_ns(ns, CONFIGMAP_NAME) {
            Some(cm) if cm.data.as_ref() == Some(&data) => Ok(None),
            Some(cm) => {
                let mut cm = (*cm).clone();
                cm.data = Some(data);
                api.replace(&cm).await?;
                Ok(None)
            }
            None => {
                let mut meta = ObjectMeta::namespaced(ns, CONFIGMAP_NAME);
                meta.annotations = Some([("kubernetes.io/description".to_string(), DESCRIPTION.to_string())].into_iter().collect());
                let cm = ConfigMap { metadata: meta, data: Some(data), ..Default::default() };
                match api.create(&cm).await {
                    Ok(_) => Ok(None),
                    Err(e) if e.is_already_exists() || e.is_not_found() || e.to_string().contains("being terminated") => Ok(None),
                    Err(e) => Err(e.into()),
                }
            }
        }
    }
}
