//! Port of `plugin/pkg/auth/authorizer/rbac`: the RBAC authorizer.
//!
//! Roles, ClusterRoles and their bindings are kept in an in-memory index fed by a
//! storage watch (upstream uses shared informers), so an authorization check
//! never touches storage. A request is allowed when any ClusterRoleBinding — or,
//! for namespaced requests, any RoleBinding in that namespace — binds the user,
//! one of their groups or their ServiceAccount to a role with a matching rule.
//! RBAC never denies: no match is `NoOpinion`.

pub mod bootstrappolicy;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use k8s_api::rbac::v1::{ClusterRole, ClusterRoleBinding, PolicyRule, Role, RoleBinding, RoleRef, Subject};
use k8s_apimachinery::watch::EventType;
use k8s_apiserver::authentication::UserInfo;
use k8s_apiserver::authorization::{Attributes, Authorizer, Decision, RuleSet};
use k8s_apiserver::Storage;
use serde_json::{json, Value};

// Upstream etcd layout: `/registry/<resource>/...` (no group in the key).
const ROLES: &str = "/registry/roles/";
const CLUSTER_ROLES: &str = "/registry/clusterroles/";
const ROLE_BINDINGS: &str = "/registry/rolebindings/";
const CLUSTER_ROLE_BINDINGS: &str = "/registry/clusterrolebindings/";

#[derive(Default)]
struct Index {
    /// (namespace, name) → rules
    roles: HashMap<(String, String), Vec<PolicyRule>>,
    cluster_roles: HashMap<String, Vec<PolicyRule>>,
    /// namespace → bindings
    role_bindings: HashMap<String, Vec<RoleBinding>>,
    cluster_role_bindings: Vec<ClusterRoleBinding>,
}

/// `verbMatches` / `apiGroupMatches` / `resourceMatches` / `resourceNameMatches`.
pub fn rule_allows(rule: &PolicyRule, a: &Attributes) -> bool {
    let has = |list: &Option<Vec<String>>, v: &str| list.as_ref().is_some_and(|l| l.iter().any(|x| x == "*" || x == v));
    if !rule.verbs.iter().any(|v| v == "*" || *v == a.verb) {
        return false;
    }
    if !a.resource_request {
        return rule.non_resource_urls.as_ref().is_some_and(|urls| {
            urls.iter().any(|u| u == "*" || *u == a.path || u.strip_suffix('*').is_some_and(|p| a.path.starts_with(p)))
        });
    }
    if !has(&rule.api_groups, &a.api_group) {
        return false;
    }
    let combined = a.resource_with_sub();
    let res_ok = rule.resources.as_ref().is_some_and(|rs| {
        rs.iter().any(|r| {
            r == "*"
                || *r == combined
                || (!a.subresource.is_empty() && *r == format!("*/{}", a.subresource))
                || (*r == format!("{}/*", a.resource) && !a.subresource.is_empty())
        })
    });
    if !res_ok {
        return false;
    }
    match &rule.resource_names {
        None => true,
        Some(n) if n.is_empty() => true,
        Some(n) => !a.name.is_empty() && n.contains(&a.name),
    }
}

/// `appliesTo`: does `subject` (from a binding in `binding_ns`) cover `user`?
pub fn subject_matches(s: &Subject, user: &UserInfo, binding_ns: &str) -> bool {
    match s.kind.as_str() {
        "User" => s.name == user.name,
        "Group" => user.in_group(&s.name),
        "ServiceAccount" => {
            let ns = s.namespace.as_deref().filter(|n| !n.is_empty()).unwrap_or(binding_ns);
            user.name == format!("system:serviceaccount:{ns}:{}", s.name)
        }
        _ => false,
    }
}

fn describe(s: &Subject) -> String {
    match s.kind.as_str() {
        "ServiceAccount" => format!("ServiceAccount \"{}/{}\"", s.namespace.as_deref().unwrap_or(""), s.name),
        k => format!("{k} \"{}\"", s.name),
    }
}

pub struct RbacAuthorizer {
    index: RwLock<Index>,
}

impl RbacAuthorizer {
    /// Builds the index from storage and keeps it current in the background.
    pub async fn start(storage: Arc<dyn Storage>) -> Arc<Self> {
        let me = Arc::new(Self { index: RwLock::new(Index::default()) });
        for prefix in [ROLES, CLUSTER_ROLES, ROLE_BINDINGS, CLUSTER_ROLE_BINDINGS] {
            let rev = me.relist(storage.as_ref(), prefix).await;
            let (me2, st) = (me.clone(), storage.clone());
            tokio::spawn(async move { me2.watch_loop(st, prefix, rev).await });
        }
        me
    }

    async fn relist(&self, storage: &dyn Storage, prefix: &str) -> u64 {
        match storage.list(prefix).await {
            Ok((items, rev)) => {
                let mut idx = self.index.write().unwrap();
                match prefix {
                    ROLES => idx.roles.clear(),
                    CLUSTER_ROLES => idx.cluster_roles.clear(),
                    ROLE_BINDINGS => idx.role_bindings.clear(),
                    _ => idx.cluster_role_bindings.clear(),
                }
                for obj in items {
                    apply(&mut idx, prefix, &obj, false);
                }
                rev
            }
            Err(e) => {
                tracing::warn!("rbac: listing {prefix}: {e}");
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
                        let mut idx = self.index.write().unwrap();
                        apply(&mut idx, prefix, &ev.object, ev.type_ == EventType::Deleted);
                    }
                    // Stream ended: resume from the last revision seen.
                }
                Err(e) => {
                    // History no longer covers `rev` (410): start over from a fresh list.
                    tracing::debug!("rbac: watch {prefix}: {e}; relisting");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    rev = self.relist(storage.as_ref(), prefix).await;
                }
            }
        }
    }

    /// Calls `f` for every rule bound to `user` that applies in `namespace`
    /// (cluster-wide first); stops when `f` returns `true`.
    fn visit(&self, user: &UserInfo, namespace: &str, mut f: impl FnMut(&str, &PolicyRule) -> bool) -> bool {
        let idx = self.index.read().unwrap();
        for b in &idx.cluster_role_bindings {
            let Some(s) = b.subjects.iter().flatten().find(|s| subject_matches(s, user, "")) else { continue };
            if let Some(rules) = idx.cluster_roles.get(&b.role_ref.name) {
                let src = format!(
                    "RBAC: allowed by ClusterRoleBinding \"{}\" of ClusterRole \"{}\" to {}",
                    b.metadata.name.as_deref().unwrap_or(""),
                    b.role_ref.name,
                    describe(s)
                );
                if rules.iter().any(|r| f(&src, r)) {
                    return true;
                }
            }
        }
        if namespace.is_empty() {
            return false;
        }
        for b in idx.role_bindings.get(namespace).into_iter().flatten() {
            let Some(s) = b.subjects.iter().flatten().find(|s| subject_matches(s, user, namespace)) else { continue };
            let rules = match b.role_ref.kind.as_str() {
                "Role" => idx.roles.get(&(namespace.to_string(), b.role_ref.name.clone())),
                _ => idx.cluster_roles.get(&b.role_ref.name),
            };
            let src = format!(
                "RBAC: allowed by RoleBinding \"{}/{namespace}\" of {} \"{}\" to {}",
                b.metadata.name.as_deref().unwrap_or(""),
                b.role_ref.kind,
                b.role_ref.name,
                describe(s)
            );
            if rules.into_iter().flatten().any(|r| f(&src, r)) {
                return true;
            }
        }
        false
    }

    /// Rules of a role reference as seen from `namespace` (for escalation checks).
    pub fn role_rules(&self, namespace: &str, r: &RoleRef) -> Option<Vec<PolicyRule>> {
        let idx = self.index.read().unwrap();
        match r.kind.as_str() {
            "Role" => idx.roles.get(&(namespace.to_string(), r.name.clone())).cloned(),
            _ => idx.cluster_roles.get(&r.name).cloned(),
        }
    }

    /// All cluster roles (for the aggregation controller and tests).
    pub fn cluster_role_names(&self) -> Vec<String> {
        self.index.read().unwrap().cluster_roles.keys().cloned().collect()
    }
}

fn apply(idx: &mut Index, prefix: &str, obj: &Value, deleted: bool) {
    let meta = &obj["metadata"];
    let name = meta["name"].as_str().unwrap_or("").to_string();
    let ns = meta["namespace"].as_str().unwrap_or("").to_string();
    match prefix {
        ROLES => {
            idx.roles.remove(&(ns.clone(), name.clone()));
            if !deleted {
                if let Ok(r) = serde_json::from_value::<Role>(obj.clone()) {
                    idx.roles.insert((ns, name), r.rules.unwrap_or_default());
                }
            }
        }
        CLUSTER_ROLES => {
            idx.cluster_roles.remove(&name);
            if !deleted {
                if let Ok(r) = serde_json::from_value::<ClusterRole>(obj.clone()) {
                    idx.cluster_roles.insert(name, r.rules.unwrap_or_default());
                }
            }
        }
        ROLE_BINDINGS => {
            let list = idx.role_bindings.entry(ns).or_default();
            list.retain(|b| b.metadata.name.as_deref() != Some(name.as_str()));
            if !deleted {
                if let Ok(b) = serde_json::from_value::<RoleBinding>(obj.clone()) {
                    list.push(b);
                }
            }
        }
        _ => {
            idx.cluster_role_bindings.retain(|b| b.metadata.name.as_deref() != Some(name.as_str()));
            if !deleted {
                if let Ok(b) = serde_json::from_value::<ClusterRoleBinding>(obj.clone()) {
                    idx.cluster_role_bindings.push(b);
                }
            }
        }
    }
}

fn rule_json(r: &PolicyRule) -> Value {
    serde_json::to_value(r).unwrap_or_else(|_| json!({}))
}

#[async_trait]
impl Authorizer for RbacAuthorizer {
    async fn authorize(&self, a: &Attributes) -> Decision {
        let mut reason = String::new();
        if self.visit(&a.user, &a.namespace, |src, r| {
            let ok = rule_allows(r, a);
            if ok {
                reason = src.to_string();
            }
            ok
        }) {
            Decision::Allow(reason)
        } else {
            Decision::NoOpinion(String::new())
        }
    }

    async fn rules_for(&self, user: &UserInfo, ns: &str) -> Option<RuleSet> {
        let mut out = RuleSet::default();
        self.visit(user, ns, |_, r| {
            if r.non_resource_urls.as_ref().is_some_and(|u| !u.is_empty()) {
                out.non_resource_rules.push(json!({"verbs": r.verbs, "nonResourceURLs": r.non_resource_urls}));
            } else {
                out.resource_rules.push(rule_json(r));
            }
            false
        });
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_apiserver::MemoryStore;

    fn attrs(user: UserInfo, verb: &str, group: &str, res: &str, sub: &str, ns: &str, name: &str) -> Attributes {
        Attributes {
            user,
            verb: verb.into(),
            resource_request: true,
            api_group: group.into(),
            resource: res.into(),
            subresource: sub.into(),
            namespace: ns.into(),
            name: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn rule_matching() {
        let u = UserInfo::new("u", &[]);
        let r = PolicyRule::resources(&[""], &["pods", "pods/log"], &["get", "list"]);
        assert!(rule_allows(&r, &attrs(u.clone(), "get", "", "pods", "", "d", "p")));
        assert!(rule_allows(&r, &attrs(u.clone(), "get", "", "pods", "log", "d", "p")));
        assert!(!rule_allows(&r, &attrs(u.clone(), "get", "", "pods", "exec", "d", "p")));
        assert!(!rule_allows(&r, &attrs(u.clone(), "delete", "", "pods", "", "d", "p")));
        assert!(!rule_allows(&r, &attrs(u.clone(), "get", "apps", "pods", "", "d", "p")));
        let named = PolicyRule::resources(&[""], &["configmaps"], &["get"]).names(&["cfg"]);
        assert!(rule_allows(&named, &attrs(u.clone(), "get", "", "configmaps", "", "d", "cfg")));
        assert!(!rule_allows(&named, &attrs(u.clone(), "get", "", "configmaps", "", "d", "other")));
        let star = PolicyRule::resources(&["*"], &["*/status"], &["*"]);
        assert!(rule_allows(&star, &attrs(u.clone(), "patch", "apps", "deployments", "status", "", "x")));
        let url = PolicyRule::urls(&["/healthz", "/api/*"], &["get"]);
        let mut a = Attributes { user: u, verb: "get".into(), path: "/api/v1".into(), ..Default::default() };
        assert!(rule_allows(&url, &a));
        a.path = "/metrics".into();
        assert!(!rule_allows(&url, &a));
    }

    #[tokio::test]
    async fn bindings_and_watch() {
        let store = MemoryStore::new();
        let st: Arc<dyn Storage> = store.clone();
        st.create(
            &format!("{CLUSTER_ROLES}pod-reader"),
            json!({"metadata": {"name": "pod-reader"}, "rules": [{"verbs": ["get", "list"], "apiGroups": [""], "resources": ["pods"]}]}),
        )
        .await
        .unwrap();
        st.create(
            &format!("{ROLE_BINDINGS}dev/read"),
            json!({"metadata": {"name": "read", "namespace": "dev"}, "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "pod-reader"},
                   "subjects": [{"kind": "Group", "name": "devs"}, {"kind": "ServiceAccount", "name": "ci"}]}),
        )
        .await
        .unwrap();
        let z = RbacAuthorizer::start(st.clone()).await;
        let jane = UserInfo::new("jane", &["devs"]);
        assert!(
            matches!(z.authorize(&attrs(jane.clone(), "list", "", "pods", "", "dev", "")).await, Decision::Allow(r) if r.contains("RoleBinding \"read/dev\""))
        );
        assert!(matches!(z.authorize(&attrs(jane.clone(), "list", "", "pods", "", "prod", "")).await, Decision::NoOpinion(_)));
        let sa = UserInfo::new("system:serviceaccount:dev:ci", &[]);
        assert!(matches!(z.authorize(&attrs(sa, "get", "", "pods", "", "dev", "x")).await, Decision::Allow(_)));
        // A new ClusterRoleBinding arrives through the watch.
        st.create(
            &format!("{CLUSTER_ROLE_BINDINGS}jane-global"),
            json!({"metadata": {"name": "jane-global"}, "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "pod-reader"},
                   "subjects": [{"kind": "User", "name": "jane"}]}),
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(matches!(z.authorize(&attrs(jane.clone(), "list", "", "pods", "", "prod", "")).await, Decision::Allow(_)));
        let rules = z.rules_for(&jane, "dev").await.unwrap();
        assert_eq!(rules.resource_rules.len(), 2);
    }
}
