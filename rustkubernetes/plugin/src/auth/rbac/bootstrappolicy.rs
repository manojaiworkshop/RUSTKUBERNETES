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

use k8s_api::rbac::v1::{AggregationRule, ClusterRole, ClusterRoleBinding, PolicyRule, Role, RoleBinding, RoleRef, Subject, GROUP};
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
/// `NodeRules()`: what every kubelet may do. The Node authorizer applies them
/// (scoping secrets, configmaps, PVs, PVCs, tokens, nodes, pods and leases to the
/// node's own objects); the `system:node` ClusterRole carries the same rules.
pub fn node_rules() -> Vec<PolicyRule> {
    vec![
        // Needed to check API access. These creates are non-mutating.
        r(&["authentication.k8s.io"], &["tokenreviews"], &["create"]),
        r(&["authorization.k8s.io"], &["subjectaccessreviews", "localsubjectaccessreviews"], &["create"]),
        // Needed to build serviceLister, to populate env vars for services.
        r(&[""], &["services"], READ),
        // Nodes can register Node API objects and report status (NodeRestriction limits them to their own).
        r(&[""], &["nodes"], &["create", "get", "list", "watch"]),
        r(&[""], &["nodes/status"], &["update", "patch"]),
        r(&[""], &["nodes"], &["update", "patch"]),
        r(&[""], &["events"], &["create", "update", "patch"]),
        r(&[""], &["pods"], READ),
        // Mirror pods, pod status and evictions (NodeRestriction limits them to pods bound to the node).
        r(&[""], &["pods"], &["create", "delete"]),
        r(&[""], &["pods/status"], &["update", "patch"]),
        r(&[""], &["pods/eviction"], &["create"]),
        // Secrets and configmaps referenced by the node's pods (Node authorizer).
        r(&[""], &["secrets", "configmaps"], &["get", "list", "watch"]),
        r(&[""], &["persistentvolumeclaims", "persistentvolumes"], &["get"]),
        r(&[""], &["endpoints"], &["get"]),
        r(&["certificates.k8s.io"], &["certificatesigningrequests"], &["create", "get", "list", "watch"]),
        r(&["coordination.k8s.io"], &["leases"], &["get", "create", "update", "patch", "delete"]),
        r(&["storage.k8s.io"], &["volumeattachments"], &["get"]),
        r(&[""], &["serviceaccounts/token"], &["create"]),
        r(&[""], &["persistentvolumeclaims/status"], &["get", "update", "patch"]),
        r(&["storage.k8s.io"], &["csidrivers"], &["get", "watch", "list"]),
        r(&["storage.k8s.io"], &["csinodes"], &["get", "create", "update", "patch", "delete"]),
        r(&["node.k8s.io"], &["runtimeclasses"], &["get", "list", "watch"]),
    ]
}

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
        role("system:node", node_rules()),
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
            "system:volume-scheduler",
            vec![
                r(&[""], &["persistentvolumes"], &["get", "list", "patch", "update", "watch"]),
                r(&["storage.k8s.io"], &["storageclasses"], READ),
                r(&[""], &["persistentvolumeclaims"], &["get", "list", "patch", "update", "watch"]),
            ],
        ),
        // A role to use for bootstrapping a node's client certificates.
        role(
            "system:node-bootstrapper",
            vec![r(&["certificates.k8s.io"], &["certificatesigningrequests"], &["create", "get", "list", "watch"])],
        ),
        // A role making the csrapprover controller approve a node client CSR.
        role(
            "system:certificates.k8s.io:certificatesigningrequests:nodeclient",
            vec![r(&["certificates.k8s.io"], &["certificatesigningrequests/nodeclient"], &["create"])],
        ),
        // A role making the csrapprover controller approve a node client CSR requested by the node itself.
        role(
            "system:certificates.k8s.io:certificatesigningrequests:selfnodeclient",
            vec![r(&["certificates.k8s.io"], &["certificatesigningrequests/selfnodeclient"], &["create"])],
        ),
        role(
            "system:certificates.k8s.io:legacy-unknown-approver",
            vec![r(&["certificates.k8s.io"], &["signers"], &["approve"]).names(&["kubernetes.io/legacy-unknown"])],
        ),
        role(
            "system:certificates.k8s.io:kubelet-serving-approver",
            vec![r(&["certificates.k8s.io"], &["signers"], &["approve"]).names(&["kubernetes.io/kubelet-serving"])],
        ),
        role(
            "system:certificates.k8s.io:kube-apiserver-client-approver",
            vec![r(&["certificates.k8s.io"], &["signers"], &["approve"]).names(&["kubernetes.io/kube-apiserver-client"])],
        ),
        role(
            "system:certificates.k8s.io:kube-apiserver-client-kubelet-approver",
            vec![r(&["certificates.k8s.io"], &["signers"], &["approve"]).names(&["kubernetes.io/kube-apiserver-client-kubelet"])],
        ),
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
        subjects: Some(subjects).filter(|s| !s.is_empty()),
        role_ref: RoleRef { api_group: GROUP.into(), kind: "ClusterRole".into(), name: role.into() },
        ..Default::default()
    }
}

/// `NamespaceRoles()`: bootstrap Roles in system namespaces.
pub fn namespace_roles() -> Vec<Role> {
    let mut reader = r(&[""], &["configmaps"], &["get", "list", "watch"]);
    // This config map is exposed and contains authentication configuration information.
    reader.resource_names = Some(vec!["extension-apiserver-authentication".into()]);
    let mut m = meta("extension-apiserver-authentication-reader", &[]);
    m.namespace = Some("kube-system".into());
    vec![Role { metadata: m, rules: Some(vec![reader]), ..Default::default() }]
}

/// `NamespaceRoleBindings()`
pub fn namespace_role_bindings() -> Vec<RoleBinding> {
    let mut m = meta("system::extension-apiserver-authentication-reader", &[]);
    m.namespace = Some("kube-system".into());
    vec![RoleBinding {
        metadata: m,
        subjects: Some(vec![Subject::user("system:kube-controller-manager"), Subject::user("system:kube-scheduler")]),
        role_ref: RoleRef { api_group: GROUP.into(), kind: "Role".into(), name: "extension-apiserver-authentication-reader".into() },
        ..Default::default()
    }]
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
        // Deprecated in 1.7 with the Node authorizer: kept, with no subjects, so that
        // reconciliation never adds the system:nodes group back.
        binding("system:node", "system:node", vec![]),
        binding("system:node-proxier", "system:node-proxier", vec![Subject::user("system:kube-proxy")]),
        binding("system:kube-scheduler", "system:kube-scheduler", vec![Subject::user("system:kube-scheduler")]),
        binding("system:volume-scheduler", "system:volume-scheduler", vec![Subject::user("system:kube-scheduler")]),
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
