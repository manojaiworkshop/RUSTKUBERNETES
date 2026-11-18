//! Port of `plugin/pkg/auth/authorizer/rbac/bootstrappolicy`: the default
//! ClusterRoles and ClusterRoleBindings every cluster starts with, reconciled
//! by the API server at startup (`storage_rbac` post-start hook). Objects carry
//! `rbac.authorization.kubernetes.io/autoupdate: "true"`; set it to `"false"`
//! to keep local edits.
//!
//! Deviation: `system:kube-controller-manager` gets full access because this
//! controller manager does not use per-controller ServiceAccount credentials
//! (`--use-service-account-credentials`).

use std::collections::BTreeMap;

use k8s_api::rbac::v1::{AggregationRule, ClusterRole, ClusterRoleBinding, PolicyRule, RoleRef, Subject, GROUP};
use k8s_apimachinery::meta::{LabelSelector, ObjectMeta};

pub const AUTOUPDATE: &str = "rbac.authorization.kubernetes.io/autoupdate";
pub const AGGREGATE_TO_ADMIN: &str = "rbac.authorization.k8s.io/aggregate-to-admin";
pub const AGGREGATE_TO_EDIT: &str = "rbac.authorization.k8s.io/aggregate-to-edit";
pub const AGGREGATE_TO_VIEW: &str = "rbac.authorization.k8s.io/aggregate-to-view";

fn r(groups: &[&str], resources: &[&str], verbs: &[&str]) -> PolicyRule {
    PolicyRule::resources(groups, resources, verbs)
}

const READ: &[&str] = &["get", "list", "watch"];
const WRITE: &[&str] = &["create", "delete", "deletecollection", "patch", "update"];
const EVENTS: &[&str] = &["create", "patch", "update"];

fn meta(name: &str, labels: &[(&str, &str)]) -> ObjectMeta {
    let mut l: BTreeMap<String, String> = labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    l.insert("kubernetes.io/bootstrapping".into(), "rbac-defaults".into());
    ObjectMeta {
        name: Some(name.into()),
        labels: Some(l),
        annotations: Some([(AUTOUPDATE.to_string(), "true".to_string())].into_iter().collect()),
        ..Default::default()
    }
}

fn role(name: &str, rules: Vec<PolicyRule>) -> ClusterRole {
    ClusterRole { metadata: meta(name, &[]), rules: Some(rules), ..Default::default() }
}

fn aggregate_to(name: &str, label: &str, rules: Vec<PolicyRule>) -> ClusterRole {
    ClusterRole { metadata: meta(name, &[(label, "true")]), rules: Some(rules), ..Default::default() }
}

/// An aggregated role (rules are filled from roles matching `labels`).
fn aggregated(name: &str, own_label: Option<&str>, selector_labels: &[&str]) -> ClusterRole {
    let labels: Vec<(&str, &str)> = own_label.map(|l| vec![(l, "true")]).unwrap_or_default();
    ClusterRole {
        metadata: meta(name, &labels),
        rules: Some(vec![]),
        aggregation_rule: Some(AggregationRule {
            cluster_role_selectors: Some(
                selector_labels
                    .iter()
                    .map(|l| LabelSelector {
                        match_labels: Some([(l.to_string(), "true".to_string())].into_iter().collect()),
                        ..Default::default()
                    })
                    .collect(),
            ),
        }),
        ..Default::default()
    }
}

/// `ClusterRoles()`.
pub fn cluster_roles() -> Vec<ClusterRole> {
    let view_core = &[
        "pods",
        "pods/log",
        "pods/status",
        "services",
        "services/status",
        "endpoints",
        "persistentvolumeclaims",
        "persistentvolumeclaims/status",
        "configmaps",
        "replicationcontrollers",
        "replicationcontrollers/scale",
        "replicationcontrollers/status",
        "serviceaccounts",
        "events",
        "limitranges",
        "resourcequotas",
        "resourcequotas/status",
        "namespaces",
        "namespaces/status",
        "bindings",
    ];
    let apps = &[
        "deployments",
        "deployments/scale",
        "deployments/status",
        "replicasets",
        "replicasets/scale",
        "replicasets/status",
        "statefulsets",
        "statefulsets/scale",
        "statefulsets/status",
        "daemonsets",
        "daemonsets/status",
        "controllerrevisions",
    ];
    let batch = &["jobs", "jobs/status", "cronjobs", "cronjobs/status"];
    let mut roles = vec![
        role("cluster-admin", vec![r(&["*"], &["*"], &["*"]), PolicyRule::urls(&["*"], &["*"])]),
        role(
            "system:discovery",
            vec![PolicyRule::urls(
                &["/api", "/api/*", "/apis", "/apis/*", "/healthz", "/livez", "/readyz", "/openapi", "/openapi/*", "/version", "/version/"],
                &["get"],
            )],
        ),
        role("system:public-info-viewer", vec![PolicyRule::urls(&["/healthz", "/livez", "/readyz", "/version", "/version/"], &["get"])]),
        role(
            "system:basic-user",
            vec![
                r(&["authorization.k8s.io"], &["selfsubjectaccessreviews", "selfsubjectrulesreviews"], &["create"]),
                r(&["authentication.k8s.io"], &["selfsubjectreviews"], &["create"]),
            ],
        ),
        role(
            "system:service-account-issuer-discovery",
            vec![PolicyRule::urls(
                &["/.well-known/openid-configuration", "/.well-known/openid-configuration/", "/openid/v1/jwks", "/openid/v1/jwks/"],
                &["get"],
            )],
        ),
        role(
            "system:monitoring",
            vec![PolicyRule::urls(&["/healthz", "/healthz/*", "/livez", "/livez/*", "/readyz", "/readyz/*", "/metrics"], &["get"])],
        ),
        role(
            "system:auth-delegator",
            vec![
                r(&["authentication.k8s.io"], &["tokenreviews"], &["create"]),
                r(&["authorization.k8s.io"], &["subjectaccessreviews"], &["create"]),
            ],
        ),
        aggregated("admin", None, &[AGGREGATE_TO_ADMIN]),
        aggregated("edit", Some(AGGREGATE_TO_ADMIN), &[AGGREGATE_TO_EDIT]),
        aggregated("view", Some(AGGREGATE_TO_EDIT), &[AGGREGATE_TO_VIEW]),
        aggregate_to(
            "system:aggregate-to-view",
            AGGREGATE_TO_VIEW,
            vec![
                r(&[""], view_core, READ),
                r(&["apps"], apps, READ),
                r(&["batch"], batch, READ),
                r(&["autoscaling"], &["horizontalpodautoscalers", "horizontalpodautoscalers/status"], READ),
                r(&["policy"], &["poddisruptionbudgets", "poddisruptionbudgets/status"], READ),
                r(&["networking.k8s.io"], &["ingresses", "ingresses/status", "networkpolicies"], READ),
                r(&["discovery.k8s.io"], &["endpointslices"], READ),
            ],
        ),
        aggregate_to(
            "system:aggregate-to-edit",
            AGGREGATE_TO_EDIT,
            vec![
                r(&[""], &["pods/attach", "pods/exec", "pods/portforward", "pods/proxy", "secrets", "services/proxy"], READ),
                r(&[""], &["serviceaccounts"], &["impersonate"]),
                r(&[""], &["pods", "pods/attach", "pods/exec", "pods/portforward", "pods/proxy"], WRITE),
                r(&[""], &["pods/eviction"], &["create"]),
                r(
                    &[""],
                    &[
                        "configmaps",
                        "events",
                        "persistentvolumeclaims",
                        "replicationcontrollers",
                        "replicationcontrollers/scale",
                        "secrets",
                        "serviceaccounts",
                        "services",
                        "services/proxy",
                    ],
                    WRITE,
                ),
                r(&[""], &["serviceaccounts/token"], &["create"]),
                r(
                    &["apps"],
                    &[
                        "daemonsets",
                        "deployments",
                        "deployments/rollback",
                        "deployments/scale",
                        "replicasets",
                        "replicasets/scale",
                        "statefulsets",
                        "statefulsets/scale",
                    ],
                    WRITE,
                ),
                r(&["batch"], &["cronjobs", "jobs"], WRITE),
                r(&["autoscaling"], &["horizontalpodautoscalers"], WRITE),
                r(&["policy"], &["poddisruptionbudgets"], WRITE),
                r(&["networking.k8s.io"], &["ingresses", "networkpolicies"], WRITE),
                r(&["discovery.k8s.io"], &["endpointslices"], WRITE),
                r(
                    &["coordination.k8s.io"],
                    &["leases"],
                    &["get", "list", "watch", "create", "delete", "deletecollection", "patch", "update"],
                ),
            ],
        ),
        aggregate_to(
            "system:aggregate-to-admin",
            AGGREGATE_TO_ADMIN,
            vec![
                r(&["authorization.k8s.io"], &["localsubjectaccessreviews"], &["create"]),
                r(
                    &[GROUP],
                    &["rolebindings", "roles"],
                    &["create", "delete", "deletecollection", "get", "list", "patch", "update", "watch"],
                ),
            ],
        ),
        role(
            "system:node",
            vec![
                r(&[""], &["nodes"], &["get", "list", "watch", "create", "update", "patch", "delete"]),
                r(&[""], &["nodes/status"], &["patch", "update"]),
                r(&[""], &["pods"], &["get", "list", "watch", "create", "delete"]),
                r(&[""], &["pods/status"], &["patch", "update"]),
                r(&[""], &["pods/eviction"], &["create"]),
                r(&[""], &["services", "endpoints", "configmaps", "secrets", "namespaces"], READ),
                r(&[""], &["persistentvolumeclaims", "persistentvolumes", "serviceaccounts"], &["get"]),
                r(&[""], &["serviceaccounts/token"], &["create"]),
                r(&["", "events.k8s.io"], &["events"], EVENTS),
                r(&["coordination.k8s.io"], &["leases"], &["get", "list", "watch", "create", "update", "patch", "delete"]),
                r(&["authentication.k8s.io"], &["tokenreviews"], &["create"]),
                r(&["authorization.k8s.io"], &["subjectaccessreviews", "localsubjectaccessreviews"], &["create"]),
                r(&["node.k8s.io"], &["runtimeclasses"], READ),
            ],
        ),
        role(
            "system:node-proxier",
            vec![
                r(&[""], &["endpoints", "services", "nodes"], READ),
                r(&["discovery.k8s.io"], &["endpointslices"], READ),
                r(&["", "events.k8s.io"], &["events"], EVENTS),
            ],
        ),
        role(
            "system:kube-scheduler",
            vec![
                r(&["", "events.k8s.io"], &["events"], EVENTS),
                r(&["coordination.k8s.io"], &["leases"], &["get", "list", "watch", "create", "update", "patch"]),
                r(
                    &[""],
                    &[
                        "nodes",
                        "namespaces",
                        "services",
                        "replicationcontrollers",
                        "persistentvolumeclaims",
                        "persistentvolumes",
                        "configmaps",
                    ],
                    READ,
                ),
                r(&[""], &["pods"], &["get", "list", "watch", "delete"]),
                r(&[""], &["bindings", "pods/binding"], &["create"]),
                r(&[""], &["pods/status"], &["patch", "update"]),
                r(&["apps"], &["replicasets", "statefulsets"], READ),
                r(&["policy"], &["poddisruptionbudgets"], READ),
                r(&["scheduling.k8s.io"], &["priorityclasses"], READ),
                r(&["storage.k8s.io"], &["storageclasses", "csinodes", "csidrivers", "csistoragecapacities"], READ),
                r(&["authentication.k8s.io"], &["tokenreviews"], &["create"]),
                r(&["authorization.k8s.io"], &["subjectaccessreviews"], &["create"]),
            ],
        ),
        role("system:kube-controller-manager", vec![r(&["*"], &["*"], &["*"])]),
        role(
            "system:kubelet-api-admin",
            vec![
                r(&[""], &["nodes"], &["get", "list", "watch", "proxy"]),
                r(
                    &[""],
                    &["nodes/log", "nodes/metrics", "nodes/proxy", "nodes/stats", "nodes/spec", "nodes/healthz", "nodes/pods"],
                    &["*"],
                ),
            ],
        ),
    ];
    // Bootstrap-time aggregation (the controller manager keeps it current afterwards).
    let by_label = |label: &str| -> Vec<PolicyRule> {
        roles
            .iter()
            .filter(|c| c.metadata.labels.as_ref().is_some_and(|l| l.contains_key(label)) && c.aggregation_rule.is_none())
            .flat_map(|c| c.rules.clone().unwrap_or_default())
            .collect()
    };
    let view = by_label(AGGREGATE_TO_VIEW);
    let mut edit = by_label(AGGREGATE_TO_EDIT);
    edit.extend(view.clone());
    let mut admin = by_label(AGGREGATE_TO_ADMIN);
    admin.extend(edit.clone());
    for c in roles.iter_mut() {
        match c.metadata.name.as_deref() {
            Some("view") => c.rules = Some(view.clone()),
            Some("edit") => c.rules = Some(edit.clone()),
            Some("admin") => c.rules = Some(admin.clone()),
            _ => {}
        }
    }
    roles
}

fn binding(name: &str, role: &str, subjects: Vec<Subject>) -> ClusterRoleBinding {
    ClusterRoleBinding {
        metadata: meta(name, &[]),
        subjects: Some(subjects),
        role_ref: RoleRef { api_group: GROUP.into(), kind: "ClusterRole".into(), name: role.into() },
        ..Default::default()
    }
}

/// `ClusterRoleBindings()` (plus kubeadm's `kubeadm:cluster-admins`).
pub fn cluster_role_bindings() -> Vec<ClusterRoleBinding> {
    vec![
        binding("cluster-admin", "cluster-admin", vec![Subject::group("system:masters")]),
        binding("kubeadm:cluster-admins", "cluster-admin", vec![Subject::group("kubeadm:cluster-admins")]),
        binding("system:discovery", "system:discovery", vec![Subject::group("system:authenticated")]),
        binding("system:basic-user", "system:basic-user", vec![Subject::group("system:authenticated")]),
        binding(
            "system:public-info-viewer",
            "system:public-info-viewer",
            vec![Subject::group("system:authenticated"), Subject::group("system:unauthenticated")],
        ),
        binding(
            "system:service-account-issuer-discovery",
            "system:service-account-issuer-discovery",
            vec![Subject::group("system:serviceaccounts")],
        ),
        binding("system:monitoring", "system:monitoring", vec![Subject::group("system:monitoring")]),
        binding("system:node", "system:node", vec![Subject::group("system:nodes")]),
        binding("system:node-proxier", "system:node-proxier", vec![Subject::user("system:kube-proxy")]),
        binding("system:kube-scheduler", "system:kube-scheduler", vec![Subject::user("system:kube-scheduler")]),
        binding("system:kube-controller-manager", "system:kube-controller-manager", vec![Subject::user("system:kube-controller-manager")]),
        binding("system:kubelet-api-admin", "system:kubelet-api-admin", vec![Subject::user("kube-apiserver-kubelet-client")]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregated_defaults() {
        let roles = cluster_roles();
        let get = |n: &str| roles.iter().find(|r| r.metadata.name.as_deref() == Some(n)).unwrap().rules.clone().unwrap();
        let has = |rules: &[PolicyRule], res: &str, verb: &str| {
            rules.iter().any(|r| r.resources.as_ref().is_some_and(|x| x.iter().any(|y| y == res)) && r.verbs.iter().any(|v| v == verb))
        };
        assert!(has(&get("view"), "pods", "list") && !has(&get("view"), "secrets", "get"));
        assert!(has(&get("edit"), "secrets", "get") && has(&get("edit"), "pods", "list"));
        assert!(has(&get("admin"), "rolebindings", "create") && has(&get("admin"), "deployments", "update"));
        assert!(cluster_role_bindings().iter().all(|b| roles.iter().any(|r| r.metadata.name.as_deref() == Some(b.role_ref.name.as_str()))));
    }
}
