//! Port of `pkg/controller/endpointslice` (+ `staging/k8s.io/endpointslice`
//! reconciler): Service + Pods → `discovery.k8s.io/v1` EndpointSlices.
//!
//! One slice per distinct resolved port set (named target ports can differ
//! between pods), labelled `kubernetes.io/service-name=<svc>` and owned by the
//! Service (so the garbage collector removes them with it). Terminating pods
//! are kept with `ready=false, terminating=true` so proxies can drain them.
//! Existing slices are reused, updated only when they differ, and surplus
//! slices are deleted.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use k8s_api::core::v1::{Pod, Service};
use k8s_api::discovery::v1::{Endpoint, EndpointConditions, EndpointPort, EndpointSlice, LABEL_MANAGED_BY, LABEL_SERVICE_NAME};
use k8s_apimachinery::meta::ObjectMeta;
use k8s_client_go::informers::SharedInformerFactory;
use k8s_client_go::tools::cache::{split_key, Event, Store};
use k8s_client_go::util::workqueue::WorkQueue;
use k8s_client_go::{Api, Client};

use crate::controller_utils::*;
use crate::endpoint::{backing_pods, pod_ref, resolve_ports, services_for_pod, ResolvedPort};
use crate::ControllerContext;

pub const CONTROLLER_NAME: &str = "endpointslice-controller.k8s.io";

pub struct Controller {
    client: Client,
    svc_store: Store<Service>,
    pod_store: Store<Pod>,
    slice_store: Store<EndpointSlice>,
    informers: Arc<SharedInformerFactory>,
    queue: Queue,
    resync: Duration,
}

impl Controller {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let svc_inf = ctx.informers.informer::<Service>();
        let pod_inf = ctx.informers.informer::<Pod>();
        let slice_inf = ctx.informers.informer::<EndpointSlice>();
        let c = Arc::new(Self {
            client: ctx.client.clone(),
            svc_store: svc_inf.store(),
            pod_store: pod_inf.store(),
            slice_store: slice_inf.store(),
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
        // Someone else changed one of our slices: reconcile it back.
        let me = c.clone();
        on_events(slice_inf.subscribe(), move |ev| {
            let s = ev.object();
            let labels = s.metadata.labels();
            if labels.get(LABEL_MANAGED_BY).map(String::as_str) == Some(CONTROLLER_NAME) {
                if let Some(svc) = labels.get(LABEL_SERVICE_NAME) {
                    me.queue.add(format!("{}/{svc}", s.metadata.namespace()));
                }
            }
        });
        c
    }

    pub async fn run(self: Arc<Self>, workers: usize) {
        self.informers.wait_for_cache_sync().await;
        let store = self.svc_store.clone();
        resync_every(self.queue.clone(), self.resync, move || store.keys());
        let me = self.clone();
        run_workers("endpointslice-controller", self.queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.sync(&k).await }
        })
        .await;
    }

    fn owned_slices(&self, ns: &str, svc: &str) -> Vec<Arc<EndpointSlice>> {
        let mut v: Vec<Arc<EndpointSlice>> = self
            .slice_store
            .list_namespace(ns)
            .into_iter()
            .filter(|s| {
                let l = s.metadata.labels();
                l.get(LABEL_SERVICE_NAME).map(String::as_str) == Some(svc)
                    && l.get(LABEL_MANAGED_BY).map(String::as_str) == Some(CONTROLLER_NAME)
            })
            .collect();
        v.sort_by(|a, b| a.metadata.name.cmp(&b.metadata.name));
        v
    }

    async fn sync(&self, key: &str) -> SyncResult {
        let (ns, name) = split_key(key);
        let Some(svc) = self.svc_store.get(key) else { return Ok(None) }; // slices are GC'd via ownerRefs
        if svc.spec.as_ref().and_then(|s| s.selector.as_ref()).is_none_or(|s| s.is_empty()) {
            return Ok(None);
        }
        let mut groups: BTreeMap<Vec<ResolvedPort>, Vec<Endpoint>> = BTreeMap::new();
        for p in backing_pods(&svc, self.pod_store.list_namespace(ns)) {
            let terminating = p.metadata.deletion_timestamp.is_some();
            let ready = is_pod_ready(&p);
            groups.entry(resolve_ports(&svc, &p)).or_default().push(Endpoint {
                addresses: vec![p.status.as_ref().and_then(|s| s.pod_ip.clone()).unwrap_or_default()],
                conditions: Some(EndpointConditions {
                    ready: Some(ready && !terminating),
                    serving: Some(ready),
                    terminating: Some(terminating),
                }),
                hostname: p.spec.as_ref().and_then(|s| s.hostname.clone()),
                target_ref: Some(pod_ref(&p)),
                node_name: p.spec.as_ref().and_then(|s| s.node_name.clone()),
                zone: None,
                deprecated_topology: None,
            });
        }
        if groups.is_empty() {
            // Keep one empty slice carrying the ports so consumers see the service.
            groups.insert(vec![], vec![]);
        }

        let api: Api<EndpointSlice> = Api::namespaced(self.client.clone(), ns);
        let existing = self.owned_slices(ns, name);
        let mut labels = svc.metadata.labels();
        labels.insert(LABEL_SERVICE_NAME.into(), name.into());
        labels.insert(LABEL_MANAGED_BY.into(), CONTROLLER_NAME.into());
        let svc_ports = svc.spec.as_ref().and_then(|s| s.ports.clone()).unwrap_or_default();

        for (i, (ports, mut endpoints)) in groups.into_iter().enumerate() {
            endpoints.sort_by(|a, b| a.addresses.cmp(&b.addresses));
            let ports: Vec<EndpointPort> = if ports.is_empty() && endpoints.is_empty() {
                svc_ports
                    .iter()
                    .map(|p| EndpointPort {
                        name: p.name.clone(),
                        protocol: p.protocol.clone(),
                        port: None,
                        app_protocol: p.app_protocol.clone(),
                    })
                    .collect()
            } else {
                ports
                    .into_iter()
                    .map(|(n, port, proto)| EndpointPort { name: n, protocol: Some(proto), port: Some(port), app_protocol: None })
                    .collect()
            };
            let desired_ports = if ports.is_empty() { None } else { Some(ports) };
            match existing.get(i) {
                Some(cur) => {
                    if cur.endpoints == endpoints && cur.ports == desired_ports && cur.metadata.labels() == labels {
                        continue;
                    }
                    let mut s = (**cur).clone();
                    s.endpoints = endpoints;
                    s.ports = desired_ports;
                    s.metadata.labels = Some(labels.clone());
                    match api.replace(&s).await {
                        Ok(_) => {}
                        Err(e) if e.is_conflict() || e.is_not_found() => return Ok(Some(Duration::from_millis(200))),
                        Err(e) => return Err(e.into()),
                    }
                }
                None => {
                    let mut s = EndpointSlice::new(ObjectMeta {
                        generate_name: Some(format!("{name}-")),
                        namespace: Some(ns.into()),
                        labels: Some(labels.clone()),
                        owner_references: Some(vec![new_controller_ref(svc.as_ref())]),
                        ..Default::default()
                    });
                    s.address_type = "IPv4".into();
                    s.endpoints = endpoints;
                    s.ports = desired_ports;
                    match api.create(&s).await {
                        Ok(_) => {}
                        Err(e) if e.to_string().contains("being terminated") => return Ok(None),
                        Err(e) => return Err(e.into()),
                    }
                }
            }
        }
        // Delete surplus slices.
        let used = existing.len().min(self.desired_count(&svc, ns));
        for s in existing.iter().skip(used) {
            match api.delete(s.metadata.name()).await {
                Ok(_) => {}
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(None)
    }

    fn desired_count(&self, svc: &Service, ns: &str) -> usize {
        let mut sets: Vec<Vec<ResolvedPort>> =
            backing_pods(svc, self.pod_store.list_namespace(ns)).iter().map(|p| resolve_ports(svc, p)).collect();
        sets.sort();
        sets.dedup();
        sets.len().max(1)
    }
}
