//! Port of `k8s.io/kubernetes/pkg/controlplane` (`instance.go`).
//!
//! * [`build_registry`] is the equivalent of `StorageProviders`: every
//!   built-in resource with its strategy, short names and subresources.
//! * [`new_server`] builds the API server (storage + admission + registry).
//! * [`bootstrap`] runs the post-start hooks: system namespaces, the
//!   `default/kubernetes` Service + Endpoints + EndpointSlice
//!   (`controller/kubernetesservice`, `controller/systemnamespaces`) and the RBAC
//!   bootstrap policy (`storage_rbac` post-start hook).

use std::sync::Arc;

use k8s_apimachinery::errors::ApiError;
use k8s_apiserver::admission::AdmissionChain;
use k8s_apiserver::authentication::AuthenticatorChain;
use k8s_apiserver::authorization::Authorizer;
use k8s_apiserver::registry::{DefaultStrategy, ResourceBuilder};
use k8s_apiserver::{ApiServer, ApiServerConfig, Registry, Storage};
use kube_plugin::admission::defaulttolerationseconds::DefaultTolerationSeconds;
use kube_plugin::admission::namespace::lifecycle::NamespaceLifecycle;
use kube_plugin::admission::priority::Priority;
use kube_plugin::admission::serviceaccount::ServiceAccount as ServiceAccountAdmission;
use kube_plugin::auth::rbac::bootstrappolicy;
use kube_registry::authentication::{AuthnReviewStrategy, Review as AuthnReview};
use kube_registry::authorization::{Review, ReviewStrategy};
use kube_registry::core::service::{IpAllocator, PortAllocator, ServiceStrategy};
use kube_registry::core::serviceaccount::ServiceAccountStrategy;
use kube_registry::rbac::{Kind as RbacKind, RbacStrategy};
use kube_registry::{apps, batch, core};
use kube_serviceaccount::TokenGenerator;
use serde_json::{json, Value};

/// Kubernetes version this server reports (keeps `kubectl` version-skew warnings away).
pub const KUBE_VERSION: &str = "v1.35.0";

pub struct Config {
    /// `--service-cluster-ip-range`
    pub service_cidr: String,
    /// `--service-node-port-range`
    pub node_port_range: String,
    /// Address and port advertised to clients (`/api` discovery, `kubernetes` endpoints).
    pub advertise_address: String,
    pub advertise_port: u16,
    /// Authentication; `None` = insecure development mode (everyone is `system:admin`).
    pub authenticator: Option<Arc<AuthenticatorChain>>,
    /// Authorization; `None` = `AlwaysAllow`.
    pub authorizer: Option<Arc<dyn Authorizer>>,
    /// TLS to kubelets (`--kubelet-client-certificate`, `--kubelet-certificate-authority`).
    pub kubelet_tls: Option<Arc<rustls::ClientConfig>>,
    /// ServiceAccount token issuing (`--service-account-signing-key-file`).
    pub service_accounts: Option<ServiceAccountConfig>,
    /// Static JSON documents served at fixed paths (OIDC discovery, JWKS).
    pub extra_paths: Vec<(String, Value)>,
}

#[derive(Clone)]
pub struct ServiceAccountConfig {
    pub generator: Arc<TokenGenerator>,
    /// `--api-audiences`
    pub audiences: Vec<String>,
    /// `--service-account-max-token-expiration`
    pub max_expiration: Option<i64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            service_cidr: "10.96.0.0/12".into(),
            node_port_range: "30000-32767".into(),
            advertise_address: "127.0.0.1".into(),
            advertise_port: 6443,
            authenticator: None,
            authorizer: None,
            kubelet_tls: None,
            service_accounts: None,
            extra_paths: vec![],
        }
    }
}

/// Registers every built-in resource (`StorageProviders`).
pub fn build_registry(cfg: &Config) -> Result<Registry, String> {
    let mut r = Registry::new();
    let service = Arc::new(ServiceStrategy::new(IpAllocator::new(&cfg.service_cidr)?, PortAllocator::new(&cfg.node_port_range)?));

    // core/v1
    r.register(
        ResourceBuilder::new("", "v1", "Namespace", "namespaces", false)
            .short_names(&["ns"])
            .subresources(&["status", "finalize"])
            .strategy(Arc::new(core::namespace::NamespaceStrategy))
            .build(),
    );
    r.register(
        ResourceBuilder::new("", "v1", "Pod", "pods", true)
            .short_names(&["po"])
            .categories(&["all"])
            .subresources(&["status", "binding", "log", "exec", "attach"])
            .strategy(Arc::new(core::pod::PodStrategy { kubelet_https: cfg.kubelet_tls.is_some() }))
            .build(),
    );
    r.register(
        ResourceBuilder::new("", "v1", "Node", "nodes", false)
            .short_names(&["no"])
            .subresources(&["status"])
            .strategy(Arc::new(core::node::strategy()))
            .build(),
    );
    r.register(
        ResourceBuilder::new("", "v1", "Service", "services", true)
            .short_names(&["svc"])
            .categories(&["all"])
            .subresources(&["status"])
            .strategy(service)
            .build(),
    );
    r.register(
        ResourceBuilder::new("", "v1", "Endpoints", "endpoints", true)
            .short_names(&["ep"])
            .strategy(Arc::new(core::simple::endpoints()))
            .build(),
    );
    r.register(
        ResourceBuilder::new("", "v1", "ConfigMap", "configmaps", true)
            .short_names(&["cm"])
            .strategy(Arc::new(core::simple::configmap()))
            .build(),
    );
    r.register(ResourceBuilder::new("", "v1", "Secret", "secrets", true).strategy(Arc::new(core::simple::secret())).build());
    r.register(
        ResourceBuilder::new("", "v1", "ServiceAccount", "serviceaccounts", true)
            .short_names(&["sa"])
            .subresources(&["token"])
            .strategy(Arc::new(ServiceAccountStrategy {
                tokens: cfg.service_accounts.as_ref().map(|s| s.generator.clone()),
                audiences: cfg.service_accounts.as_ref().map(|s| s.audiences.clone()).unwrap_or_default(),
                max_expiration: cfg.service_accounts.as_ref().and_then(|s| s.max_expiration),
            }))
            .build(),
    );
    r.register(
        ResourceBuilder::new("", "v1", "Event", "events", true).short_names(&["ev"]).strategy(Arc::new(core::simple::event())).build(),
    );

    // apps/v1
    r.register(
        ResourceBuilder::new("apps", "v1", "Deployment", "deployments", true)
            .short_names(&["deploy"])
            .categories(&["all"])
            .subresources(&["status", "scale"])
            .strategy(Arc::new(apps::deployment()))
            .build(),
    );
    r.register(
        ResourceBuilder::new("apps", "v1", "ReplicaSet", "replicasets", true)
            .short_names(&["rs"])
            .categories(&["all"])
            .subresources(&["status", "scale"])
            .strategy(Arc::new(apps::replicaset()))
            .build(),
    );
    r.register(
        ResourceBuilder::new("apps", "v1", "DaemonSet", "daemonsets", true)
            .short_names(&["ds"])
            .categories(&["all"])
            .subresources(&["status"])
            .strategy(Arc::new(apps::daemonset()))
            .build(),
    );
    r.register(
        ResourceBuilder::new("apps", "v1", "StatefulSet", "statefulsets", true)
            .short_names(&["sts"])
            .categories(&["all"])
            .subresources(&["status", "scale"])
            .strategy(Arc::new(apps::statefulset()))
            .build(),
    );
    r.register(
        ResourceBuilder::new("apps", "v1", "ControllerRevision", "controllerrevisions", true)
            .strategy(Arc::new(DefaultStrategy { has_status: false }))
            .build(),
    );

    // batch/v1 (stored now, controllers in a later phase)
    r.register(
        ResourceBuilder::new("batch", "v1", "Job", "jobs", true)
            .categories(&["all"])
            .subresources(&["status"])
            .strategy(Arc::new(batch::job()))
            .build(),
    );
    r.register(
        ResourceBuilder::new("batch", "v1", "CronJob", "cronjobs", true)
            .short_names(&["cj"])
            .categories(&["all"])
            .subresources(&["status"])
            .strategy(Arc::new(batch::cronjob()))
            .build(),
    );

    // scheduling.k8s.io/v1
    r.register(
        ResourceBuilder::new("scheduling.k8s.io", "v1", "PriorityClass", "priorityclasses", false)
            .short_names(&["pc"])
            .strategy(Arc::new(batch::priorityclass()))
            .build(),
    );

    // coordination.k8s.io/v1, discovery.k8s.io/v1
    r.register(
        ResourceBuilder::new("coordination.k8s.io", "v1", "Lease", "leases", true).strategy(Arc::new(core::simple::lease())).build(),
    );
    r.register(
        ResourceBuilder::new("discovery.k8s.io", "v1", "EndpointSlice", "endpointslices", true)
            .strategy(Arc::new(batch::endpointslice()))
            .build(),
    );

    // rbac.authorization.k8s.io/v1
    let rbac = "rbac.authorization.k8s.io";
    for (kind, plural, namespaced, k) in [
        ("Role", "roles", true, RbacKind::Role),
        ("ClusterRole", "clusterroles", false, RbacKind::ClusterRole),
        ("RoleBinding", "rolebindings", true, RbacKind::RoleBinding),
        ("ClusterRoleBinding", "clusterrolebindings", false, RbacKind::ClusterRoleBinding),
    ] {
        r.register(ResourceBuilder::new(rbac, "v1", kind, plural, namespaced).strategy(Arc::new(RbacStrategy(k))).build());
    }

    // authorization.k8s.io/v1 and authentication.k8s.io/v1 (create-only reviews)
    for (kind, plural, namespaced, rv) in [
        ("SelfSubjectAccessReview", "selfsubjectaccessreviews", false, Review::SelfSubjectAccess),
        ("SubjectAccessReview", "subjectaccessreviews", false, Review::SubjectAccess),
        ("LocalSubjectAccessReview", "localsubjectaccessreviews", true, Review::LocalSubjectAccess),
        ("SelfSubjectRulesReview", "selfsubjectrulesreviews", false, Review::SelfSubjectRules),
    ] {
        r.register(
            ResourceBuilder::new("authorization.k8s.io", "v1", kind, plural, namespaced).strategy(Arc::new(ReviewStrategy(rv))).build(),
        );
    }
    for (kind, plural, rv) in
        [("SelfSubjectReview", "selfsubjectreviews", AuthnReview::SelfSubject), ("TokenReview", "tokenreviews", AuthnReview::Token)]
    {
        r.register(
            ResourceBuilder::new("authentication.k8s.io", "v1", kind, plural, false).strategy(Arc::new(AuthnReviewStrategy(rv))).build(),
        );
    }
    Ok(r)
}

/// Builds the API server (does not start serving).
pub fn new_server(cfg: &Config, storage: Arc<dyn Storage>) -> Result<Arc<ApiServer>, String> {
    let registry = build_registry(cfg)?;
    let mut plugins: Vec<Arc<dyn k8s_apiserver::admission::AdmissionPlugin>> = vec![Arc::new(NamespaceLifecycle)];
    // Pods only get API credentials when the server can issue tokens.
    if cfg.service_accounts.is_some() {
        plugins.push(Arc::new(ServiceAccountAdmission));
    }
    plugins.push(Arc::new(Priority));
    plugins.push(Arc::new(DefaultTolerationSeconds));
    Ok(ApiServer::new(ApiServerConfig {
        registry,
        storage,
        admission: AdmissionChain::new(plugins),
        version: version_info(),
        advertise_address: format!("{}:{}", cfg.advertise_address, cfg.advertise_port),
        protobuf_decoder: Some(k8s_api::protobuf::decode),
        authenticator: cfg.authenticator.clone(),
        authorizer: cfg.authorizer.clone(),
        kubelet_tls: cfg.kubelet_tls.clone(),
        extra_paths: cfg.extra_paths.clone(),
    }))
}

/// `/version` body.
pub fn version_info() -> Value {
    let v = KUBE_VERSION.trim_start_matches('v');
    let mut parts = v.split('.');
    json!({
        "major": parts.next().unwrap_or("1"),
        "minor": parts.next().unwrap_or("0"),
        "gitVersion": format!("{KUBE_VERSION}+rustkubernetes-{}", env!("CARGO_PKG_VERSION")),
        "gitCommit": "rustkubernetes",
        "gitTreeState": "clean",
        "buildDate": "1970-01-01T00:00:00Z",
        "goVersion": "rust",
        "compiler": "rustc",
        "platform": format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
    })
}

/// Creates an object unless it already exists (bootstrap is idempotent across restarts).
async fn ensure(server: &Arc<ApiServer>, group: &str, plural: &str, ns: &str, obj: Value) -> Result<(), ApiError> {
    let info = server.registry.lookup(group, "v1", plural).ok_or_else(|| ApiError::internal(format!("{plural} not registered")))?;
    let name = obj.pointer("/metadata/name").and_then(Value::as_str).unwrap_or("").to_string();
    if server.storage.get(&info.key(ns, &name)).await.is_ok() {
        return Ok(());
    }
    match server.create_internal(group, "v1", plural, ns, obj).await {
        Ok(_) => Ok(()),
        Err(e) if e.is_already_exists() => Ok(()),
        Err(e) => Err(e),
    }
}

/// Creates `obj`, or rewrites `fields` of the existing object when they differ.
async fn upsert(server: &Arc<ApiServer>, group: &str, plural: &str, ns: &str, obj: Value, fields: &[&str]) -> Result<(), ApiError> {
    let info = server.registry.lookup(group, "v1", plural).ok_or_else(|| ApiError::internal(format!("{plural} not registered")))?;
    let name = obj.pointer("/metadata/name").and_then(Value::as_str).unwrap_or("").to_string();
    let key = info.key(ns, &name);
    match server.storage.get(&key).await {
        Err(_) => ensure(server, group, plural, ns, obj).await,
        Ok(mut cur) => {
            if fields.iter().all(|f| cur.get(*f) == obj.get(*f)) {
                return Ok(());
            }
            for f in fields {
                cur[*f] = obj[*f].clone();
            }
            server.storage.update(&key, None, cur).await.map(|_| ()).map_err(|e| ApiError::internal(e.to_string()))
        }
    }
}

/// `storage_rbac` post-start hook: creates missing bootstrap roles and bindings and
/// updates those still marked `autoupdate: "true"`.
async fn reconcile_rbac(server: &Arc<ApiServer>) -> Result<(), ApiError> {
    let g = "rbac.authorization.k8s.io";
    let wanted: Vec<(&str, Value, &[&str])> = bootstrappolicy::cluster_roles()
        .into_iter()
        .map(|r| ("clusterroles", serde_json::to_value(r).unwrap_or_default(), &["rules", "aggregationRule"][..]))
        .chain(
            bootstrappolicy::cluster_role_bindings()
                .into_iter()
                .map(|b| ("clusterrolebindings", serde_json::to_value(b).unwrap_or_default(), &["subjects", "roleRef"][..])),
        )
        .collect();
    let mut created = 0;
    for (plural, obj, fields) in wanted {
        let info = server.registry.lookup(g, "v1", plural).ok_or_else(|| ApiError::internal(format!("{plural} not registered")))?;
        let name = obj.pointer("/metadata/name").and_then(Value::as_str).unwrap_or("").to_string();
        match server.storage.get(&info.key("", &name)).await {
            Err(_) => {
                ensure(server, g, plural, "", obj).await?;
                created += 1;
            }
            Ok(cur) => {
                let auto = cur
                    .pointer(&format!("/metadata/annotations/{}", bootstrappolicy::AUTOUPDATE.replace('/', "~1")))
                    .and_then(Value::as_str);
                // Aggregated roles' rules are owned by the aggregation controller.
                let fields: Vec<&str> = fields
                    .iter()
                    .copied()
                    .filter(|f| !(*f == "rules" && obj.get("aggregationRule").is_some_and(|a| !a.is_null())))
                    .collect();
                if auto != Some("false") {
                    upsert(server, g, plural, "", obj, &fields).await?;
                }
            }
        }
    }
    if created > 0 {
        tracing::info!(created, "created RBAC bootstrap policy");
    }
    Ok(())
}

/// Post-start hooks: allocator repair, system namespaces, `kubernetes` service.
pub async fn bootstrap(server: &Arc<ApiServer>, cfg: &Config) -> Result<(), ApiError> {
    server.restore().await?;
    for ns in ["default", "kube-system", "kube-public", "kube-node-lease"] {
        ensure(server, "", "namespaces", "", json!({"metadata": {"name": ns}})).await?;
    }
    // Built-in priority classes (`scheduling/bootstrap`).
    for (name, value, desc) in [
        ("system-node-critical", 2_000_001_000, "Used for system critical pods that must not be moved from their current node."),
        (
            "system-cluster-critical",
            2_000_000_000,
            "Used for system critical pods that must run in the cluster, but can be moved to another node if necessary.",
        ),
    ] {
        ensure(
            server,
            "scheduling.k8s.io",
            "priorityclasses",
            "",
            json!({"metadata": {"name": name}, "value": value, "description": desc}),
        )
        .await?;
    }
    // default/kubernetes Service with the first IP of the service CIDR.
    let first_ip = IpAllocator::new(&cfg.service_cidr).map_err(ApiError::internal)?.first().to_string();
    ensure(
        server,
        "",
        "services",
        "default",
        json!({
                    "metadata": {"name": "kubernetes", "labels": {"component": "apiserver", "provider": "kubernetes"}},
                    "spec": {"clusterIP": first_ip, "ports": [
                        {"name": "https", "port": 443, "protocol": "TCP", "targetPort": cfg.advertise_port}]},
        }),
    )
    .await?;
    // Endpoints of the `kubernetes` Service: this server (`EndpointReconciler`).
    upsert(
        server,
        "",
        "endpoints",
        "default",
        json!({
                    "metadata": {"name": "kubernetes", "labels": {"endpointslice.kubernetes.io/skip-mirror": "true"}},
                    "subsets": [{"addresses": [{"ip": cfg.advertise_address}],
                                 "ports": [{"name": "https", "port": cfg.advertise_port, "protocol": "TCP"}]}],
        }),
        &["subsets"],
    )
    .await?;
    upsert(
        server,
        "discovery.k8s.io",
        "endpointslices",
        "default",
        json!({
                    "metadata": {"name": "kubernetes", "labels": {"kubernetes.io/service-name": "kubernetes"}},
                    "addressType": "IPv4",
                    "endpoints": [{"addresses": [cfg.advertise_address], "conditions": {"ready": true}}],
                    "ports": [{"name": "https", "port": cfg.advertise_port, "protocol": "TCP"}],
        }),
        &["endpoints", "ports"],
    )
    .await?;
    reconcile_rbac(server).await?;
    server.set_ready();
    tracing::info!("bootstrap complete");
    Ok(())
}
