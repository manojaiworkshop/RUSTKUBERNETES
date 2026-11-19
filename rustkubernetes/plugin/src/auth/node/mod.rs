//! Port of `plugin/pkg/auth/authorizer/node`: the Node authorizer
//! (`--authorization-mode=Node`).
//!
//! Requests from kubelets (user `system:node:<name>` in group `system:nodes`)
//! are authorized here, and only here: since Kubernetes 1.8 the `system:node`
//! ClusterRoleBinding has no subjects. A node may:
//!
//! * read secrets, configmaps, PVCs and PVs only when a pod bound to it (or a PV
//!   it mounts) references them, and create tokens only for ServiceAccounts of
//!   its pods ([`graph`]);
//! * read only its own Node object, and list/watch pods only with
//!   `fieldSelector=spec.nodeName=<itself>`;
//! * get, update, patch and delete only its own Lease in `kube-node-lease`;
//! * do whatever `NodeRules` allow for everything else.
//!
//! Writes that the authorizer cannot scope by name (creating its Node, mirror
//! pods, leases, pod status) are narrowed by the `NodeRestriction` admission
//! plugin. Nothing here ever denies: other authorizers (RBAC) may still allow.

pub mod graph;

use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use k8s_api::rbac::v1::PolicyRule;
use k8s_apimachinery::watch::EventType;
use k8s_apiserver::authorization::{Attributes, Authorizer, Decision};
use k8s_apiserver::Storage;
use serde_json::Value;

use super::nodeidentifier::node_identity;
use super::rbac::{bootstrappolicy, rule_allows};
use graph::{Graph, Kind};

const PODS: &str = "/registry/pods/";
const PVS: &str = "/registry/persistentvolumes/";
/// `api.NamespaceNodeLease`
pub const NAMESPACE_NODE_LEASE: &str = "kube-node-lease";

pub struct NodeAuthorizer {
    graph: RwLock<Graph>,
    node_rules: Vec<PolicyRule>,
}

fn allow() -> Decision {
    Decision::Allow(String::new())
}

fn no(reason: impl Into<String>) -> Decision {
    Decision::NoOpinion(reason.into())
}

impl NodeAuthorizer {
    /// Builds the graph from storage and keeps it current (`AddGraphEventHandlers`).
    pub async fn start(storage: Arc<dyn Storage>) -> Arc<Self> {
        let me = Arc::new(Self { graph: RwLock::new(Graph::default()), node_rules: bootstrappolicy::node_rules() });
        for prefix in [PODS, PVS] {
            let rev = me.relist(storage.as_ref(), prefix).await;
            let (me2, st) = (me.clone(), storage.clone());
            tokio::spawn(async move { me2.watch_loop(st, prefix, rev).await });
        }
        me
    }

    /// A graph without a storage feed (tests).
    pub fn with_graph(graph: Graph) -> Self {
        Self { graph: RwLock::new(graph), node_rules: bootstrappolicy::node_rules() }
    }

    fn apply(&self, prefix: &str, obj: &Value, deleted: bool) {
        let ns = obj.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or("");
        let name = obj.pointer("/metadata/name").and_then(Value::as_str).unwrap_or("");
        let mut g = self.graph.write().unwrap();
        match prefix {
            PODS => g.set_pod(ns, name, (!deleted).then_some(obj)),
            _ => g.set_pv(name, (!deleted).then_some(obj)),
        }
    }

    async fn relist(&self, storage: &dyn Storage, prefix: &str) -> u64 {
        match storage.list(prefix).await {
            Ok((items, rev)) => {
                {
                    let mut g = self.graph.write().unwrap();
                    if prefix == PODS {
                        g.clear_pods();
                    } else {
                        g.clear_pvs();
                    }
                }
                for obj in &items {
                    self.apply(prefix, obj, false);
                }
                rev
            }
            Err(e) => {
                tracing::warn!("node authorizer: listing {prefix}: {e}");
                0
            }
        }
    }

    async fn watch_loop(self: Arc<Self>, storage: Arc<dyn Storage>, prefix: &'static str, mut rev: u64) {
        loop {
            match storage.watch(prefix, rev).await {
                Ok(mut w) => {
                    while let Some(Ok(ev)) = w.next().await {
                        rev = ev.revision;
                        self.apply(prefix, &ev.object, ev.type_ == EventType::Deleted);
                    }
                }
                Err(e) => {
                    tracing::debug!("node authorizer: watch {prefix}: {e}; relisting");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    rev = self.relist(storage.as_ref(), prefix).await;
                }
            }
        }
    }

    fn deny_log(node: &str, a: &Attributes) {
        tracing::debug!(node, verb = %a.verb, resource = %a.resource_with_sub(), namespace = %a.namespace, name = %a.name, "NODE DENY");
    }

    /// `authorize`: a path from the named object to the node.
    fn authorize_path(&self, node: &str, kind: Kind, a: &Attributes) -> Decision {
        if a.name.is_empty() {
            Self::deny_log(node, a);
            return no("No Object name found");
        }
        if !self.graph.read().unwrap().has_path(node, kind, &a.namespace, &a.name) {
            Self::deny_log(node, a);
            return no(format!("no relationship found between node '{node}' and this object"));
        }
        allow()
    }

    /// `authorizeReadNamespacedObject` (secrets, configmaps).
    fn authorize_read_namespaced(&self, node: &str, kind: Kind, a: &Attributes) -> Decision {
        if !matches!(a.verb.as_str(), "get" | "list" | "watch") {
            Self::deny_log(node, a);
            return no("can only read resources of this type");
        }
        if !a.subresource.is_empty() {
            return no("cannot read subresource");
        }
        if a.namespace.is_empty() {
            return no("can only read namespaced object of this type");
        }
        self.authorize_path(node, kind, a)
    }

    /// `authorizeGet` (PVs, PVCs).
    fn authorize_get(&self, node: &str, kind: Kind, a: &Attributes) -> Decision {
        if a.verb != "get" {
            Self::deny_log(node, a);
            return no("can only get individual resources of this type");
        }
        if !a.subresource.is_empty() {
            return no("cannot get subresource");
        }
        self.authorize_path(node, kind, a)
    }

    /// `authorizeStatusUpdate` (persistentvolumeclaims/status).
    fn authorize_status_update(&self, node: &str, kind: Kind, a: &Attributes) -> Decision {
        if !matches!(a.verb.as_str(), "update" | "patch") {
            Self::deny_log(node, a);
            return no("can only get/update/patch this type");
        }
        if a.subresource != "status" {
            return no("can only update status subresource");
        }
        self.authorize_path(node, kind, a)
    }

    /// `authorizeCreateToken` (serviceaccounts/token).
    fn authorize_create_token(&self, node: &str, a: &Attributes) -> Decision {
        if a.verb != "create" || a.name.is_empty() {
            Self::deny_log(node, a);
            return no("can only create tokens for individual service accounts");
        }
        if a.subresource != "token" {
            return no("can only create token subresource of serviceaccount");
        }
        self.authorize_path(node, Kind::ServiceAccount, a)
    }

    /// `authorizeLease`: the node's own Lease in `kube-node-lease`.
    fn authorize_lease(node: &str, a: &Attributes) -> Decision {
        if !matches!(a.verb.as_str(), "get" | "create" | "update" | "patch" | "delete") {
            Self::deny_log(node, a);
            return no("can only get, create, update, patch, or delete a node lease");
        }
        if a.namespace != NAMESPACE_NODE_LEASE {
            return no(format!("can only access leases in the {NAMESPACE_NODE_LEASE:?} system namespace"));
        }
        // The name is unknown on create; NodeRestriction checks it there.
        if a.verb != "create" && a.name != node {
            return no("can only access node lease with the same name as the requesting node");
        }
        allow()
    }

    /// `authorizeCSINode`: the node's own CSINode object.
    fn authorize_csi_node(node: &str, a: &Attributes) -> Decision {
        if !matches!(a.verb.as_str(), "get" | "create" | "update" | "patch" | "delete") {
            return no("can only get, create, update, patch, or delete a CSINode");
        }
        if !a.subresource.is_empty() {
            return no("cannot authorize CSINode subresources");
        }
        if a.verb != "create" && a.name != node {
            return no("can only access CSINode with the same name as the requesting node");
        }
        allow()
    }

    /// `authorizeNode` (`AuthorizeNodeWithSelectors`): only its own Node object.
    fn authorize_node(node: &str, a: &Attributes) -> Decision {
        match (a.subresource.as_str(), a.verb.as_str()) {
            // Use NodeRestriction to limit a node to creating/updating its own object.
            ("", "create" | "update" | "patch") => allow(),
            // Compare the name directly so kubelets can read their Node before creating it.
            ("", "get" | "list" | "watch") => {
                if a.name == node {
                    allow()
                } else if a.name.is_empty() {
                    no(format!("node '{node}' cannot read all nodes, only its own Node object"))
                } else {
                    no(format!("node '{node}' cannot read '{}', only its own Node object", a.name))
                }
            }
            ("status", "update" | "patch") => allow(),
            _ => {
                Self::deny_log(node, a);
                no("")
            }
        }
    }

    /// `authorizePod` (`AuthorizePodWithSelectors`).
    fn authorize_pod(&self, node: &str, a: &Attributes) -> Decision {
        match (a.subresource.as_str(), a.verb.as_str()) {
            ("", "get") => self.authorize_path(node, Kind::Pod, a),
            ("", "list" | "watch") => {
                if a.field_requirements().iter().any(|(f, v)| *f == "spec.nodeName" && *v == node) {
                    allow()
                } else {
                    Self::deny_log(node, a);
                    no("can only list/watch pods with spec.nodeName field selector")
                }
            }
            // Mirror pods; NodeRestriction limits them to the node.
            ("", "create" | "delete") => allow(),
            ("status", "update" | "patch") => allow(),
            ("eviction", "create") => allow(),
            _ => {
                Self::deny_log(node, a);
                no("")
            }
        }
    }
}

#[async_trait]
impl Authorizer for NodeAuthorizer {
    async fn authorize(&self, a: &Attributes) -> Decision {
        let (node, is_node) = node_identity(&a.user);
        if !is_node {
            return no("");
        }
        if node.is_empty() {
            tracing::debug!(user = %a.user.name, "NODE DENY: unknown node");
            return no(format!("unknown node for user {:?}", a.user.name));
        }
        if a.resource_request {
            match (a.api_group.as_str(), a.resource.as_str()) {
                ("", "secrets") => return self.authorize_read_namespaced(&node, Kind::Secret, a),
                ("", "configmaps") => return self.authorize_read_namespaced(&node, Kind::ConfigMap, a),
                ("", "persistentvolumeclaims") => {
                    return if a.subresource == "status" {
                        self.authorize_status_update(&node, Kind::Pvc, a)
                    } else {
                        self.authorize_get(&node, Kind::Pvc, a)
                    }
                }
                ("", "persistentvolumes") => return self.authorize_get(&node, Kind::Pv, a),
                ("", "serviceaccounts") => return self.authorize_create_token(&node, a),
                ("coordination.k8s.io", "leases") => return Self::authorize_lease(&node, a),
                ("storage.k8s.io", "csinodes") => return Self::authorize_csi_node(&node, a),
                ("", "nodes") => return Self::authorize_node(&node, a),
                ("", "pods") => return self.authorize_pod(&node, a),
                _ => {}
            }
        }
        // Everything else is not subdivided: the static node rules.
        if self.node_rules.iter().any(|r| rule_allows(r, a)) {
            return allow();
        }
        no("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_apiserver::authentication::UserInfo;
    use serde_json::json;

    fn authz() -> NodeAuthorizer {
        let mut g = Graph::default();
        let pod = json!({"metadata": {"namespace": "ns", "name": "p"}, "spec": {"nodeName": "n1", "serviceAccountName": "sa",
            "volumes": [{"name": "s", "secret": {"secretName": "s1"}}]}});
        g.set_pod("ns", "p", Some(&pod));
        NodeAuthorizer::with_graph(g)
    }

    fn req(user: &str, verb: &str, group: &str, resource: &str, sub: &str, ns: &str, name: &str) -> Attributes {
        Attributes {
            user: UserInfo::new(user, &["system:nodes", "system:authenticated"]),
            verb: verb.into(),
            resource_request: true,
            api_group: group.into(),
            api_version: "v1".into(),
            resource: resource.into(),
            subresource: sub.into(),
            namespace: ns.into(),
            name: name.into(),
            ..Default::default()
        }
    }

    async fn ok(z: &NodeAuthorizer, a: Attributes) -> bool {
        matches!(z.authorize(&a).await, Decision::Allow(_))
    }

    async fn reason(z: &NodeAuthorizer, a: Attributes) -> String {
        match z.authorize(&a).await {
            Decision::NoOpinion(r) | Decision::Deny(r) => r,
            Decision::Allow(_) => "allowed".into(),
        }
    }

    #[tokio::test]
    async fn secrets_follow_the_graph() {
        let z = authz();
        assert!(ok(&z, req("system:node:n1", "get", "", "secrets", "", "ns", "s1")).await);
        assert!(ok(&z, req("system:node:n1", "watch", "", "secrets", "", "ns", "s1")).await);
        assert_eq!(
            reason(&z, req("system:node:n2", "get", "", "secrets", "", "ns", "s1")).await,
            "no relationship found between node 'n2' and this object"
        );
        assert_eq!(reason(&z, req("system:node:n1", "list", "", "secrets", "", "ns", "")).await, "No Object name found");
        assert_eq!(
            reason(&z, req("system:node:n1", "update", "", "secrets", "", "ns", "s1")).await,
            "can only read resources of this type"
        );
        assert!(ok(&z, req("system:node:n1", "create", "", "serviceaccounts", "token", "ns", "sa")).await);
        assert!(!ok(&z, req("system:node:n1", "create", "", "serviceaccounts", "token", "ns", "default")).await);
    }

    #[tokio::test]
    async fn nodes_pods_leases() {
        let z = authz();
        assert!(ok(&z, req("system:node:n1", "get", "", "nodes", "", "", "n1")).await);
        assert_eq!(
            reason(&z, req("system:node:n1", "get", "", "nodes", "", "", "n2")).await,
            "node 'n1' cannot read 'n2', only its own Node object"
        );
        assert!(!ok(&z, req("system:node:n1", "list", "", "nodes", "", "", "")).await);
        assert!(ok(&z, req("system:node:n1", "patch", "", "nodes", "status", "", "n1")).await);
        assert!(!ok(&z, req("system:node:n1", "delete", "", "nodes", "", "", "n1")).await);
        let mut list = req("system:node:n1", "watch", "", "pods", "", "", "");
        list.field_selector = "spec.nodeName=n1".into();
        assert!(ok(&z, list.clone()).await);
        list.field_selector = "spec.nodeName=n2".into();
        assert!(!ok(&z, list).await);
        assert!(ok(&z, req("system:node:n1", "get", "", "pods", "", "ns", "p")).await);
        assert!(!ok(&z, req("system:node:n2", "get", "", "pods", "", "ns", "p")).await);
        assert!(ok(&z, req("system:node:n1", "update", "coordination.k8s.io", "leases", "", "kube-node-lease", "n1")).await);
        assert!(!ok(&z, req("system:node:n1", "update", "coordination.k8s.io", "leases", "", "kube-node-lease", "n2")).await);
        assert!(!ok(&z, req("system:node:n1", "get", "coordination.k8s.io", "leases", "", "kube-system", "n1")).await);
    }

    #[tokio::test]
    async fn static_rules_and_non_nodes() {
        let z = authz();
        assert!(ok(&z, req("system:node:n1", "list", "", "services", "", "ns", "")).await);
        assert!(ok(&z, req("system:node:n1", "create", "", "events", "", "ns", "")).await);
        assert!(ok(&z, req("system:node:n1", "create", "authorization.k8s.io", "subjectaccessreviews", "", "", "")).await);
        assert!(!ok(&z, req("system:node:n1", "list", "", "namespaces", "", "", "")).await);
        assert!(!ok(&z, req("system:node:n1", "create", "apps", "deployments", "", "ns", "")).await);
        let mut other = req("system:node:n1", "get", "", "secrets", "", "ns", "s1");
        other.user = UserInfo::new("system:node:n1", &["system:authenticated"]);
        assert_eq!(reason(&z, other).await, "");
        let mut unknown = req("system:node:", "get", "", "nodes", "", "", "x");
        unknown.user = UserInfo::new("system:node:", &["system:nodes"]);
        assert!(reason(&z, unknown).await.contains("unknown node for user"));
    }
}
