//! Port of `pkg/controller/endpoint`: Service + Pods → `v1.Endpoints`.
//!
//! For every Service with a selector, pods with an IP that match the selector
//! become addresses (`notReadyAddresses` when not ready, unless
//! `publishNotReadyAddresses`). Named `targetPort`s are resolved per pod, so
//! pods exposing different ports end up in different subsets.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use k8s_api::core::v1::{EndpointAddress, EndpointPort, EndpointSubset, Endpoints, ObjectReference, Pod, Service};
use k8s_api::IntOrString;
use k8s_apimachinery::labels::Selector;
use k8s_apimachinery::meta::ObjectMeta;
use k8s_client_go::informers::SharedInformerFactory;
use k8s_client_go::tools::cache::{split_key, Event, Store};
use k8s_client_go::util::workqueue::WorkQueue;
use k8s_client_go::{Api, Client};

use crate::controller_utils::*;
use crate::ControllerContext;

/// `(name, port, protocol)` of a service port resolved against one pod.
pub type ResolvedPort = (Option<String>, i32, String);

/// Resolves every service port's `targetPort` for `pod` (`podutil.FindPort`).
pub fn resolve_ports(svc: &Service, pod: &Pod) -> Vec<ResolvedPort> {
    let mut out = vec![];
    for sp in svc.spec.as_ref().and_then(|s| s.ports.clone()).unwrap_or_default() {
        let proto = sp.protocol.clone().unwrap_or_else(|| "TCP".into());
        let port = match sp.target_port.clone().unwrap_or(IntOrString::Int(sp.port as i64)) {
            IntOrString::Int(p) => Some(p as i32),
            IntOrString::String(name) => pod
                .spec
                .as_ref()
                .into_iter()
                .flat_map(|s| s.containers.iter())
                .flat_map(|c| c.ports.iter().flatten())
                .find(|p| p.name.as_deref() == Some(name.as_str()) && p.protocol.as_deref().unwrap_or("TCP") == proto)
                .map(|p| p.container_port),
        };
        if let Some(p) = port {
            out.push((sp.name.clone(), p, proto));
        }
    }
    out
}

/// Pods that back a service: matching selector, with an IP, not finished.
pub fn backing_pods(svc: &Service, pods: Vec<Arc<Pod>>) -> Vec<Arc<Pod>> {
    let Some(sel) = svc.spec.as_ref().and_then(|s| s.selector.clone()).filter(|s| !s.is_empty()) else { return vec![] };
    let sel = Selector::from_map(&sel);
    pods.into_iter()
        .filter(|p| sel.matches(&p.metadata.labels()))
        .filter(|p| p.status.as_ref().and_then(|s| s.pod_ip.as_ref()).is_some_and(|ip| !ip.is_empty()))
        .filter(|p| !matches!(pod_phase(p), "Succeeded" | "Failed"))
        .collect()
}

/// Services in the pod's namespace that select it.
pub fn services_for_pod(store: &Store<Service>, pod: &Pod) -> Vec<String> {
    let labels = pod.metadata.labels();
    store
        .list_namespace(pod.metadata.namespace())
        .into_iter()
        .filter(|s| {
            s.spec
                .as_ref()
                .and_then(|sp| sp.selector.as_ref())
                .is_some_and(|sel| !sel.is_empty() && Selector::from_map(sel).matches(&labels))
        })
        .map(|s| key(s.as_ref()))
        .collect()
}

pub fn pod_ref(p: &Pod) -> ObjectReference {
    ObjectReference {
        kind: Some("Pod".into()),
        namespace: p.metadata.namespace.clone(),
        name: p.metadata.name.clone(),
        uid: p.metadata.uid.clone(),
        ..Default::default()
    }
}

pub struct Controller {
    client: Client,
    svc_store: Store<Service>,
    pod_store: Store<Pod>,
    ep_store: Store<Endpoints>,
    informers: Arc<SharedInformerFactory>,
    queue: Queue,
    resync: Duration,
}

impl Controller {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let svc_inf = ctx.informers.informer::<Service>();
        let pod_inf = ctx.informers.informer::<Pod>();
        let ep_inf = ctx.informers.informer::<Endpoints>();
        let c = Arc::new(Self {
            client: ctx.client.clone(),
            svc_store: svc_inf.store(),
            pod_store: pod_inf.store(),
            ep_store: ep_inf.store(),
            informers: ctx.informers.clone(),
            queue: WorkQueue::new(),
            resync: ctx.config.resync_period,
        });
        let me = c.clone();
        on_events(svc_inf.subscribe(), move |ev| me.queue.add(key(ev.object().as_ref())));
        let me = c.clone();
        on_events(pod_inf.subscribe(), move |ev| {
            if let Event::Updated { old, .. } = &ev {
                services_for_pod(&me.svc_store, old).into_iter().for_each(|k| me.queue.add(k));
            }
            services_for_pod(&me.svc_store, ev.object()).into_iter().for_each(|k| me.queue.add(k));
        });
        c
    }

    pub async fn run(self: Arc<Self>, workers: usize) {
        self.informers.wait_for_cache_sync().await;
        let store = self.svc_store.clone();
        resync_every(self.queue.clone(), self.resync, move || store.keys());
        let me = self.clone();
        run_workers("endpoints-controller", self.queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.sync(&k).await }
        })
        .await;
    }

    async fn sync(&self, key: &str) -> SyncResult {
        let (ns, name) = split_key(key);
        let api: Api<Endpoints> = Api::namespaced(self.client.clone(), ns);
        let Some(svc) = self.svc_store.get(key) else {
            match api.delete(name).await {
                Ok(_) => {}
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(e.into()),
            }
            return Ok(None);
        };
        // Services without a selector manage their own Endpoints.
        if svc.spec.as_ref().and_then(|s| s.selector.as_ref()).is_none_or(|s| s.is_empty()) {
            return Ok(None);
        }
        let publish_not_ready = svc.spec.as_ref().and_then(|s| s.publish_not_ready_addresses).unwrap_or(false);
        // Group addresses by their resolved port set.
        let mut groups: BTreeMap<Vec<ResolvedPort>, (Vec<EndpointAddress>, Vec<EndpointAddress>)> = BTreeMap::new();
        for p in backing_pods(&svc, self.pod_store.list_namespace(ns)) {
            if p.metadata.deletion_timestamp.is_some() && !publish_not_ready {
                continue;
            }
            let ports = resolve_ports(&svc, &p);
            let addr = EndpointAddress {
                ip: p.status.as_ref().and_then(|s| s.pod_ip.clone()).unwrap_or_default(),
                hostname: p.spec.as_ref().and_then(|s| s.hostname.clone()),
                node_name: p.spec.as_ref().and_then(|s| s.node_name.clone()),
                target_ref: Some(pod_ref(&p)),
            };
            let entry = groups.entry(ports).or_default();
            if is_pod_ready(&p) || publish_not_ready {
                entry.0.push(addr);
            } else {
                entry.1.push(addr);
            }
        }
        let mut subsets: Vec<EndpointSubset> = groups
            .into_iter()
            .filter(|(ports, _)| !ports.is_empty())
            .map(|(ports, (mut ready, mut not_ready))| {
                ready.sort_by(|a, b| a.ip.cmp(&b.ip));
                not_ready.sort_by(|a, b| a.ip.cmp(&b.ip));
                EndpointSubset {
                    addresses: Some(ready).filter(|v| !v.is_empty()),
                    not_ready_addresses: Some(not_ready).filter(|v| !v.is_empty()),
                    ports: Some(
                        ports.into_iter().map(|(name, port, protocol)| EndpointPort { name, port, protocol: Some(protocol) }).collect(),
                    ),
                }
            })
            .collect();
        subsets.sort_by_key(|s| serde_json::to_string(s).unwrap_or_default());

        let existing = self.ep_store.get(key);
        let mut ep = existing.as_deref().cloned().unwrap_or_else(|| Endpoints::new(ObjectMeta::namespaced(ns, name)));
        let labels = svc.metadata.labels.clone();
        let new_subsets = if subsets.is_empty() { None } else { Some(subsets) };
        if existing.is_some() && ep.subsets == new_subsets && ep.metadata.labels == labels {
            return Ok(None);
        }
        ep.subsets = new_subsets;
        ep.metadata.labels = labels;
        let res = if existing.is_some() { api.replace(&ep).await } else { api.create(&ep).await };
        match res {
            Ok(_) => Ok(None),
            Err(e) if e.is_already_exists() || e.is_conflict() => Ok(Some(Duration::from_millis(200))),
            Err(e) if e.to_string().contains("being terminated") => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
