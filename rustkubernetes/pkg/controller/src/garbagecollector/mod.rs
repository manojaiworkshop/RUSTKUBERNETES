//! Port of `pkg/controller/garbagecollector`.
//!
//! The GC watches **every** deletable resource with dynamic informers and keeps
//! a graph `uid → {owners}` plus the reverse `owner uid → {dependents}`
//! (`graph_builder.go`). Work items:
//!
//! * `attemptToDelete(uid)`: if all owners of an object are gone (verified
//!   against the API server), delete it (background). If some owners remain,
//!   only drop the dangling references. For an object being deleted with the
//!   `foregroundDeletion` finalizer: delete its dependents first and remove the
//!   finalizer once none are left.
//! * `attemptToOrphan(uid)`: for an object deleted with the `orphan`
//!   finalizer, remove the ownerReference from every dependent, then remove
//!   the finalizer.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use k8s_apimachinery::meta::OwnerReference;
use k8s_apimachinery::unstructured::ObjectExt;
use k8s_client_go::api::{ApiResource, DynamicApi};
use k8s_client_go::discovery::server_preferred_resources;
use k8s_client_go::tools::cache::{DynamicObject, Event, Informer};
use k8s_client_go::util::workqueue::WorkQueue;
use k8s_client_go::{Client, DeleteParams, ListParams, PatchType};
use serde_json::{json, Value};

use crate::controller_utils::*;
use crate::ControllerContext;

pub const FOREGROUND: &str = "foregroundDeletion";
pub const ORPHAN: &str = "orphan";

/// Resources the GC never monitors (`ignoredResources`).
const IGNORED: &[&str] = &["events"];
/// How often deletable resources are re-discovered (`garbageCollector.Sync` period),
/// so that custom resources are monitored once their CRD is established.
const DISCOVERY_RESYNC: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
struct Node {
    resource: ApiResource,
    namespace: String,
    name: String,
    owners: Vec<OwnerReference>,
    deleting: bool,
    finalizers: Vec<String>,
}

#[derive(Default)]
struct Graph {
    nodes: HashMap<String, Node>,
    dependents: HashMap<String, HashSet<String>>,
}

impl Graph {
    fn upsert(&mut self, uid: &str, node: Node) {
        if let Some(old) = self.nodes.get(uid) {
            for o in &old.owners {
                if let Some(d) = self.dependents.get_mut(&o.uid) {
                    d.remove(uid);
                }
            }
        }
        for o in &node.owners {
            self.dependents.entry(o.uid.clone()).or_default().insert(uid.to_string());
        }
        self.nodes.insert(uid.to_string(), node);
    }

    fn remove(&mut self, uid: &str) -> Option<Node> {
        let n = self.nodes.remove(uid)?;
        for o in &n.owners {
            if let Some(d) = self.dependents.get_mut(&o.uid) {
                d.remove(uid);
            }
        }
        Some(n)
    }

    fn dependents_of(&self, uid: &str) -> Vec<String> {
        self.dependents.get(uid).map(|s| s.iter().filter(|d| self.nodes.contains_key(*d)).cloned().collect()).unwrap_or_default()
    }
}

pub struct Controller {
    client: Client,
    graph: Mutex<Graph>,
    resources: Mutex<Vec<ApiResource>>,
    informers: Mutex<Vec<(ApiResource, Arc<Informer<DynamicObject>>)>>,
    queue: Queue,
}

fn node_from(resource: &ApiResource, obj: &Value) -> Node {
    Node {
        resource: resource.clone(),
        namespace: obj.namespace().to_string(),
        name: obj.name().to_string(),
        owners: obj.pointer("/metadata/ownerReferences").and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default(),
        deleting: obj.deletion_timestamp().is_some(),
        finalizers: obj.finalizers(),
    }
}

impl Controller {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        Arc::new(Self {
            client: ctx.client.clone(),
            graph: Mutex::new(Graph::default()),
            resources: Mutex::new(vec![]),
            informers: Mutex::new(vec![]),
            queue: WorkQueue::new(),
        })
    }

    /// Deletable resources from discovery.
    async fn discover(&self) -> anyhow::Result<Vec<ApiResource>> {
        let resources = server_preferred_resources(&self.client).await?;
        Ok(resources
            .into_iter()
            .filter(|r| r.supports("delete") && r.supports("list") && r.supports("watch") && !IGNORED.contains(&r.resource.plural.as_str()))
            .map(|r| r.resource)
            .collect())
    }

    /// `resyncMonitors`: starts informers for new resources and stops those of
    /// resources that went away. Returns whether anything changed.
    async fn sync_monitors(self: &Arc<Self>, monitored: Vec<ApiResource>) -> bool {
        let current = self.resources.lock().unwrap().clone();
        let added: Vec<ApiResource> = monitored.iter().filter(|r| !current.contains(r)).cloned().collect();
        let removed: Vec<ApiResource> = current.iter().filter(|r| !monitored.contains(r)).cloned().collect();
        *self.resources.lock().unwrap() = monitored;
        {
            let mut infs = self.informers.lock().unwrap();
            infs.retain(|(r, inf)| {
                let keep = !removed.contains(r);
                if !keep {
                    inf.stop();
                }
                keep
            });
        }
        let mut started = vec![];
        for r in &added {
            let inf = Informer::dynamic(DynamicApi::new(self.client.clone(), r.clone(), None), ListParams::default());
            let me = self.clone();
            let res = r.clone();
            on_events(inf.subscribe(), move |ev| me.handle(&res, ev));
            inf.start();
            self.informers.lock().unwrap().push((r.clone(), inf.clone()));
            started.push(inf);
        }
        for i in &started {
            i.wait_synced().await;
        }
        if !removed.is_empty() || !added.is_empty() {
            tracing::info!(
                added = ?added.iter().map(|r| r.plural.as_str()).collect::<Vec<_>>(),
                removed = ?removed.iter().map(|r| r.plural.as_str()).collect::<Vec<_>>(),
                "garbage collector resynced monitors"
            );
        }
        !removed.is_empty() || !added.is_empty()
    }

    /// Queues every object that has owners (or is being deleted).
    fn enqueue_all(&self) {
        let with_owners: Vec<String> = {
            let g = self.graph.lock().unwrap();
            g.nodes.iter().filter(|(_, n)| !n.owners.is_empty() || n.deleting).map(|(u, _)| format!("d:{u}")).collect()
        };
        for k in with_owners {
            self.queue.add(k);
        }
    }

    pub async fn run(self: Arc<Self>, workers: usize) {
        // Discover deletable resources (retry until the API server answers).
        let monitored = loop {
            match self.discover().await {
                Ok(r) => break r,
                Err(e) => {
                    tracing::warn!("garbage collector discovery failed: {e}");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        };
        self.sync_monitors(monitored).await;
        tracing::info!(resources = self.informers.lock().unwrap().len(), "garbage collector synced");
        // Initial pass over everything that has owners.
        self.enqueue_all();
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(DISCOVERY_RESYNC).await;
                match me.discover().await {
                    Ok(r) => {
                        if me.sync_monitors(r).await {
                            me.enqueue_all();
                        }
                    }
                    Err(e) => tracing::debug!("garbage collector discovery failed: {e}"),
                }
            }
        });
        let me = self.clone();
        run_workers("garbage-collector-controller", self.queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.process(&k).await }
        })
        .await;
    }

    fn handle(&self, resource: &ApiResource, ev: Event<DynamicObject>) {
        let obj = &ev.object().0;
        let uid = obj.uid().to_string();
        if uid.is_empty() {
            return;
        }
        let mut g = self.graph.lock().unwrap();
        match ev {
            Event::Deleted(_) => {
                let removed = g.remove(&uid);
                for d in g.dependents_of(&uid) {
                    self.queue.add(format!("d:{d}"));
                }
                // Owners deleting in foreground may now be unblocked.
                for o in removed.map(|n| n.owners).unwrap_or_default() {
                    if g.nodes.get(&o.uid).is_some_and(|n| n.deleting) {
                        self.queue.add(format!("d:{}", o.uid));
                    }
                }
            }
            Event::Added(_) | Event::Updated { .. } => {
                let node = node_from(resource, obj);
                let deleting = node.deleting;
                let has_owners = !node.owners.is_empty();
                let orphan = node.finalizers.iter().any(|f| f == ORPHAN);
                let fg = node.finalizers.iter().any(|f| f == FOREGROUND);
                g.upsert(&uid, node);
                if deleting && orphan {
                    self.queue.add(format!("o:{uid}"));
                } else if (deleting && fg) || has_owners {
                    self.queue.add(format!("d:{uid}"));
                }
            }
        }
    }

    fn api_for(&self, node: &Node) -> DynamicApi {
        DynamicApi::new(self.client.clone(), node.resource.clone(), Some(&node.namespace).filter(|n| !n.is_empty()).map(String::as_str))
    }

    /// Resource for an owner reference (`apiVersion` + `kind`).
    fn resource_for(&self, owner: &OwnerReference) -> Option<ApiResource> {
        let (group, version) = owner.api_version.split_once('/').unwrap_or(("", &owner.api_version));
        self.resources.lock().unwrap().iter().find(|r| r.group == group && r.version == version && r.kind == owner.kind).cloned()
    }

    async fn process(&self, item: &str) -> SyncResult {
        match item.split_once(':') {
            Some(("d", uid)) => self.attempt_to_delete(uid).await,
            Some(("o", uid)) => self.attempt_to_orphan(uid).await,
            _ => Ok(None),
        }
    }

    /// Owner state: Some(true) = exists and blocks (deleting in foreground),
    /// Some(false) = exists, None = gone.
    async fn owner_state(&self, dep: &Node, owner: &OwnerReference) -> anyhow::Result<Option<bool>> {
        if let Some(n) = self.graph.lock().unwrap().nodes.get(&owner.uid) {
            return Ok(Some(n.deleting && n.finalizers.iter().any(|f| f == FOREGROUND)));
        }
        // Not in the graph: confirm with the API server before acting (informer lag).
        let Some(r) = self.resource_for(owner) else { return Ok(Some(false)) };
        let ns = if r.namespaced { Some(dep.namespace.as_str()) } else { None };
        match DynamicApi::new(self.client.clone(), r, ns).get(&owner.name).await {
            Ok(o) if o.uid() == owner.uid => Ok(Some(o.deletion_timestamp().is_some() && o.finalizers().iter().any(|f| f == FOREGROUND))),
            Ok(_) => Ok(None), // same name, different object
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn attempt_to_delete(&self, uid: &str) -> SyncResult {
        let Some(node) = self.graph.lock().unwrap().nodes.get(uid).cloned() else { return Ok(None) };
        let api = self.api_for(&node);

        if node.deleting {
            if !node.finalizers.iter().any(|f| f == FOREGROUND) {
                return Ok(None);
            }
            // Foreground deletion: delete dependents, then remove our finalizer.
            let deps: Vec<Node> = {
                let g = self.graph.lock().unwrap();
                g.dependents_of(uid).iter().filter_map(|d| g.nodes.get(d).cloned()).collect()
            };
            if deps.is_empty() {
                let left: Vec<&String> = node.finalizers.iter().filter(|f| *f != FOREGROUND).collect();
                match api.patch(&node.name, PatchType::Merge, &json!({"metadata": {"finalizers": left}})).await {
                    Ok(_) => tracing::info!(object = %format!("{}/{}", node.resource.plural, node.name), "foreground deletion complete"),
                    Err(e) if e.is_not_found() => {}
                    Err(e) => return Err(e.into()),
                }
                return Ok(None);
            }
            for d in deps.iter().filter(|d| !d.deleting) {
                let has_children = !self.graph.lock().unwrap().dependents_of_name(d).is_empty();
                let dp = if has_children { DeleteParams::foreground() } else { DeleteParams::background() };
                match self.api_for(d).delete_with(&d.name, &dp).await {
                    Ok(_) => {}
                    Err(e) if e.is_not_found() => {}
                    Err(e) => return Err(e.into()),
                }
            }
            return Ok(Some(Duration::from_secs(1)));
        }

        if node.owners.is_empty() {
            return Ok(None);
        }
        let (mut solid, mut waiting, mut dangling) = (vec![], vec![], vec![]);
        for o in &node.owners {
            match self.owner_state(&node, o).await? {
                Some(false) => solid.push(o.clone()),
                Some(true) => waiting.push(o.clone()),
                None => dangling.push(o.clone()),
            }
        }
        if !solid.is_empty() {
            if !dangling.is_empty() {
                let keep: Vec<&OwnerReference> = node.owners.iter().filter(|o| !dangling.iter().any(|d| d.uid == o.uid)).collect();
                match api.patch(&node.name, PatchType::Merge, &json!({"metadata": {"ownerReferences": keep}})).await {
                    Ok(_) => {}
                    Err(e) if e.is_not_found() => {}
                    Err(e) => return Err(e.into()),
                }
            }
            return Ok(None);
        }
        let has_dependents = !self.graph.lock().unwrap().dependents_of(uid).is_empty();
        let dp = if !waiting.is_empty() && has_dependents { DeleteParams::foreground() } else { DeleteParams::background() };
        let dp = DeleteParams { precondition_uid: Some(uid.to_string()), ..dp };
        match api.delete_with(&node.name, &dp).await {
            Ok(_) => {
                tracing::info!(object = %format!("{}/{}/{}", node.resource.plural, node.namespace, node.name), "deleted object whose owners are gone")
            }
            Err(e) if e.is_not_found() || e.is_conflict() => {}
            Err(e) => return Err(e.into()),
        }
        Ok(None)
    }

    async fn attempt_to_orphan(&self, uid: &str) -> SyncResult {
        let Some(node) = self.graph.lock().unwrap().nodes.get(uid).cloned() else { return Ok(None) };
        let deps: Vec<Node> = {
            let g = self.graph.lock().unwrap();
            g.dependents_of(uid).iter().filter_map(|d| g.nodes.get(d).cloned()).collect()
        };
        for d in deps {
            let keep: Vec<&OwnerReference> = d.owners.iter().filter(|o| o.uid != uid).collect();
            match self.api_for(&d).patch(&d.name, PatchType::Merge, &json!({"metadata": {"ownerReferences": keep}})).await {
                Ok(_) => {}
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(e.into()),
            }
        }
        let left: Vec<&String> = node.finalizers.iter().filter(|f| *f != ORPHAN).collect();
        match self.api_for(&node).patch(&node.name, PatchType::Merge, &json!({"metadata": {"finalizers": left}})).await {
            Ok(_) => tracing::info!(object = %node.name, "orphaned dependents"),
            Err(e) if e.is_not_found() => {}
            Err(e) => return Err(e.into()),
        }
        Ok(None)
    }
}

impl Graph {
    fn dependents_of_name(&self, node: &Node) -> Vec<String> {
        self.nodes
            .iter()
            .find(|(_, n)| n.name == node.name && n.namespace == node.namespace && n.resource == node.resource)
            .map(|(u, _)| self.dependents_of(u))
            .unwrap_or_default()
    }
}
