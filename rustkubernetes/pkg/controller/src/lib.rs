//! Port of `k8s.io/kubernetes/pkg/controller`: the built-in controllers.
//!
//! | Module | Controller name | Reconciles |
//! |--------|-----------------|------------|
//! | [`replicaset`] | `replicaset-controller` | ReplicaSet → Pods |
//! | [`deployment`] | `deployment-controller` | Deployment → ReplicaSets (RollingUpdate / Recreate) |
//! | [`statefulset`] | `statefulset-controller` | StatefulSet → ordered Pods |
//! | [`daemon`] | `daemonset-controller` | DaemonSet → one Pod per node |
//! | [`job`] | `job-controller` | Job → Pods until completion |
//! | [`cronjob`] | `cronjob-controller` | CronJob → Jobs on a schedule |
//! | [`ttlafterfinished`] | `ttl-after-finished-controller` | deletes finished Jobs |
//! | [`namespace`] | `namespace-controller` | deletes content of terminating namespaces |
//! | [`garbagecollector`] | `garbage-collector-controller` | ownerReference cascading deletion |
//! | [`serviceaccount`] | `serviceaccount-controller` | `default` ServiceAccount per namespace |
//! | [`endpoint`] | `endpoints-controller` | Service + Pods → Endpoints |
//! | [`endpointslice`] | `endpointslice-controller` | Service + Pods → EndpointSlices |
//! | [`nodelifecycle`] | `node-lifecycle-controller`, `taint-eviction-controller` | node health, NoExecute eviction |
//! | [`nodeipam`] | `node-ipam-controller` | `spec.podCIDRs` of each node (`--allocate-node-cidrs`) |
//! | [`volume::persistentvolume`] | `persistentvolume-binder-controller` | PVC ↔ PV binding, provisioning, reclaim |
//! | [`volume::protection`] | `persistentvolumeclaim-protection-controller`, `persistentvolume-protection-controller` | storage protection finalizers |
//! | [`volume::ephemeral`] | `ephemeral-volume-controller` | PVCs for generic ephemeral volumes |
//! | [`certificates::signer`] | `certificatesigningrequest-signing-controller` | signs approved CSRs |
//! | [`certificates::approver`] | `certificatesigningrequest-approving-controller` | auto-approves kubelet client CSRs |
//! | [`certificates::cleaner`] | `certificatesigningrequest-cleaner-controller` | deletes old CSRs |
//! | [`bootstrap`] | `bootstrap-signer-controller`, `token-cleaner-controller` (off by default) | signs `cluster-info`, deletes expired bootstrap tokens |
//! | [`podgc`] | `pod-garbage-collector-controller` | orphaned / terminated pods |
//! | [`rootcacertpublisher`] | `root-ca-certificate-publisher-controller` | `kube-root-ca.crt` ConfigMap per namespace |
//! | [`clusterroleaggregation`] | `clusterrole-aggregation-controller` | rules of aggregated ClusterRoles |
//!
//! Every controller follows the same pattern as in Go: informers → event
//! handlers that enqueue keys → a rate-limited work queue → `sync(key)`.

pub mod bootstrap;
pub mod certificates;
use certificates::cleaner as cleaner_alias;
pub mod clusterroleaggregation;
pub mod controller_utils;
pub mod cronjob;
pub mod daemon;
pub mod deployment;
pub mod endpoint;
pub mod endpointslice;
pub mod garbagecollector;
pub mod job;
pub mod namespace;
pub mod nodeipam;
pub mod nodelifecycle;
pub mod podgc;
pub mod replicaset;
pub mod rootcacertpublisher;
pub mod serviceaccount;
pub mod statefulset;
pub mod ttlafterfinished;
pub mod volume;

use std::sync::Arc;
use std::time::Duration;

use k8s_client_go::informers::SharedInformerFactory;
use k8s_client_go::tools::record::EventRecorder;
use k8s_client_go::Client;

/// Tunables (`KubeControllerManagerConfiguration` subset).
#[derive(Clone, Debug)]
pub struct ControllerConfig {
    /// Workers per controller (`--concurrent-*-syncs`).
    pub workers: usize,
    /// How often every object is re-enqueued (`--min-resync-period`).
    pub resync_period: Duration,
    /// `--node-monitor-grace-period`
    pub node_monitor_grace_period: Duration,
    /// `--node-monitor-period`
    pub node_monitor_period: Duration,
    /// `--terminated-pod-gc-threshold`
    pub terminated_pod_gc_threshold: usize,
    /// Contents of `--root-ca-file` (published as `kube-root-ca.crt`).
    pub root_ca: Option<String>,
    /// `--allocate-node-cidrs`, `--cluster-cidr`, `--node-cidr-mask-size*`, `--service-cluster-ip-range`
    pub node_ipam: nodeipam::NodeIpamConfig,
    /// `--enable-hostpath-provisioner`, `--pvclaimbinder-sync-period`
    pub volume: VolumeConfig,
    /// `--cluster-signing-*`
    pub csr_signing: certificates::signer::CsrSigningConfig,
}

/// `PersistentVolumeBinderControllerConfiguration` subset.
#[derive(Clone, Debug)]
pub struct VolumeConfig {
    pub enable_hostpath_provisioner: bool,
    pub pv_claim_binder_sync_period: Duration,
}

impl Default for VolumeConfig {
    fn default() -> Self {
        Self { enable_hostpath_provisioner: false, pv_claim_binder_sync_period: Duration::from_secs(15) }
    }
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            workers: 5,
            resync_period: Duration::from_secs(30),
            node_monitor_grace_period: Duration::from_secs(40),
            node_monitor_period: Duration::from_secs(5),
            terminated_pod_gc_threshold: 12500,
            root_ca: None,
            node_ipam: nodeipam::NodeIpamConfig::default(),
            volume: VolumeConfig::default(),
            csr_signing: certificates::signer::CsrSigningConfig {
                cluster_signing_duration: Duration::from_secs(365 * 24 * 3600),
                ..Default::default()
            },
        }
    }
}

/// Shared state handed to every controller (`ControllerContext`).
#[derive(Clone)]
pub struct ControllerContext {
    pub client: Client,
    pub informers: Arc<SharedInformerFactory>,
    pub config: ControllerConfig,
}

impl ControllerContext {
    pub fn new(client: Client, config: ControllerConfig) -> Self {
        Self { informers: SharedInformerFactory::new(client.clone()), client, config }
    }

    pub fn recorder(&self, component: &str) -> EventRecorder {
        EventRecorder::new(self.client.clone(), component)
    }
}

/// Description of a controller (`ControllerDescriptor`).
pub struct ControllerDescriptor {
    pub name: &'static str,
    /// Old short names accepted by `--controllers` (e.g. `replicaset`).
    pub aliases: &'static [&'static str],
    pub disabled_by_default: bool,
    /// Builds the controller (subscribing to informers) and returns its run future.
    pub constructor: fn(&ControllerContext) -> futures::future::BoxFuture<'static, ()>,
}

macro_rules! descriptor {
    ($name:expr, [$($alias:expr),*], $module:ident) => {
        ControllerDescriptor {
            name: $name,
            aliases: &[$($alias),*],
            disabled_by_default: false,
            constructor: |ctx| {
                let c = $module::Controller::new(ctx);
                let workers = ctx.config.workers;
                Box::pin(async move { c.run(workers).await })
            },
        }
    };
}

/// All controllers known to kube-controller-manager (`NewControllerDescriptors`).
pub fn controller_descriptors() -> Vec<ControllerDescriptor> {
    vec![
        descriptor!("replicaset-controller", ["replicaset"], replicaset),
        descriptor!("deployment-controller", ["deployment"], deployment),
        descriptor!("statefulset-controller", ["statefulset"], statefulset),
        descriptor!("daemonset-controller", ["daemonset"], daemon),
        descriptor!("job-controller", ["job"], job),
        descriptor!("cronjob-controller", ["cronjob"], cronjob),
        descriptor!("ttl-after-finished-controller", ["ttl-after-finished"], ttlafterfinished),
        descriptor!("namespace-controller", ["namespace"], namespace),
        descriptor!("garbage-collector-controller", ["garbagecollector"], garbagecollector),
        descriptor!("serviceaccount-controller", ["serviceaccount"], serviceaccount),
        descriptor!("root-ca-certificate-publisher-controller", ["root-ca-cert-publisher"], rootcacertpublisher),
        descriptor!("clusterrole-aggregation-controller", ["clusterrole-aggregation"], clusterroleaggregation),
        descriptor!("endpoints-controller", ["endpoint"], endpoint),
        descriptor!("endpointslice-controller", ["endpointslice"], endpointslice),
        descriptor!("node-ipam-controller", ["nodeipam"], nodeipam),
        descriptor!("node-lifecycle-controller", ["nodelifecycle"], nodelifecycle),
        ControllerDescriptor {
            name: "taint-eviction-controller",
            aliases: &["taint-eviction"],
            disabled_by_default: false,
            constructor: |ctx| {
                let c = nodelifecycle::TaintEvictionController::new(ctx);
                Box::pin(async move { c.run().await })
            },
        },
        descriptor!("pod-garbage-collector-controller", ["podgc"], podgc),
        ControllerDescriptor {
            name: "certificatesigningrequest-signing-controller",
            aliases: &["csrsigning"],
            disabled_by_default: false,
            constructor: |ctx| match certificates::signer::new_controllers(ctx) {
                Ok(cs) => Box::pin(async move {
                    futures::future::join_all(cs.into_iter().map(|c| c.run(5))).await;
                }),
                Err(e) => {
                    tracing::error!("error starting \"certificatesigningrequest-signing-controller\": {e}");
                    std::process::exit(1)
                }
            },
        },
        ControllerDescriptor {
            name: "certificatesigningrequest-approving-controller",
            aliases: &["csrapproving"],
            disabled_by_default: false,
            constructor: |ctx| {
                let c = certificates::approver::new_controller(ctx);
                Box::pin(async move { c.run(5).await })
            },
        },
        descriptor!("certificatesigningrequest-cleaner-controller", ["csrcleaner"], cleaner_alias),
        ControllerDescriptor {
            name: "bootstrap-signer-controller",
            aliases: &["bootstrapsigner"],
            disabled_by_default: true,
            constructor: |ctx| {
                let c = bootstrap::Signer::new(ctx);
                Box::pin(async move { c.run(1).await })
            },
        },
        ControllerDescriptor {
            name: "token-cleaner-controller",
            aliases: &["tokencleaner"],
            disabled_by_default: true,
            constructor: |ctx| {
                let c = bootstrap::TokenCleaner::new(ctx);
                Box::pin(async move { c.run(1).await })
            },
        },
        ControllerDescriptor {
            name: "persistentvolume-binder-controller",
            aliases: &["persistentvolume-binder"],
            disabled_by_default: false,
            constructor: |ctx| {
                let c = volume::persistentvolume::Controller::new(ctx);
                let w = ctx.config.workers;
                Box::pin(async move { c.run(w).await })
            },
        },
        ControllerDescriptor {
            name: "persistentvolumeclaim-protection-controller",
            aliases: &["pvc-protection"],
            disabled_by_default: false,
            constructor: |ctx| {
                let c = volume::protection::PvcProtection::new(ctx);
                let w = ctx.config.workers;
                Box::pin(async move { c.run(w).await })
            },
        },
        ControllerDescriptor {
            name: "persistentvolume-protection-controller",
            aliases: &["pv-protection"],
            disabled_by_default: false,
            constructor: |ctx| {
                let c = volume::protection::PvProtection::new(ctx);
                let w = ctx.config.workers;
                Box::pin(async move { c.run(w).await })
            },
        },
        ControllerDescriptor {
            name: "ephemeral-volume-controller",
            aliases: &["ephemeral-volume"],
            disabled_by_default: false,
            constructor: |ctx| {
                let c = volume::ephemeral::Controller::new(ctx);
                let w = ctx.config.workers;
                Box::pin(async move { c.run(w).await })
            },
        },
    ]
}

/// Evaluates `--controllers` (`*`, `name`, `-name`) for one descriptor.
pub fn is_enabled(d: &ControllerDescriptor, flags: &[String]) -> bool {
    let matches = |f: &str| f == d.name || d.aliases.contains(&f);
    let mut star = false;
    for f in flags {
        if let Some(neg) = f.strip_prefix('-') {
            if matches(neg) {
                return false;
            }
        } else if matches(f) {
            return true;
        } else if f == "*" {
            star = true;
        }
    }
    star && !d.disabled_by_default
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_flags() {
        let all = controller_descriptors();
        let rs = all.iter().find(|d| d.name == "replicaset-controller").unwrap();
        let f = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(is_enabled(rs, &f(&["*"])));
        assert!(!is_enabled(rs, &f(&["*", "-replicaset"])));
        assert!(is_enabled(rs, &f(&["replicaset-controller"])));
        assert!(!is_enabled(rs, &f(&["deployment"])));
    }
}
