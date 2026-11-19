//! `cmd/kube-controller-manager/app/options`: command-line flags.

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "kube-controller-manager",
    version,
    about = "The Kubernetes controller manager is a daemon that embeds the core control loops shipped with Kubernetes."
)]
pub struct Options {
    /// Path to a kubeconfig file with authorization and master location information.
    #[arg(long, env = "KUBECONFIG")]
    pub kubeconfig: Option<PathBuf>,

    /// The address of the Kubernetes API server (overrides any value in kubeconfig).
    #[arg(long)]
    pub master: Option<String>,

    /// A list of controllers to enable. '*' enables all on-by-default controllers,
    /// 'foo' enables the controller named 'foo', '-foo' disables the controller named 'foo'.
    #[arg(long, value_delimiter = ',', default_value = "*")]
    pub controllers: Vec<String>,

    /// Start a leader election client and gain leadership before executing the main loop.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub leader_elect: bool,

    /// Duration non-leader candidates wait before trying to acquire leadership (seconds).
    #[arg(long, default_value_t = 15)]
    pub leader_elect_lease_duration: u64,

    /// Interval between attempts by the acting leader to renew leadership (seconds).
    #[arg(long, default_value_t = 10)]
    pub leader_elect_renew_deadline: u64,

    /// Wait between tries of actions (seconds).
    #[arg(long, default_value_t = 2)]
    pub leader_elect_retry_period: u64,

    /// Number of workers per controller (`--concurrent-*-syncs`).
    #[arg(long, default_value_t = 5)]
    pub concurrent_syncs: usize,

    /// Period after which a node without heartbeats is marked Unknown (seconds).
    #[arg(long, default_value_t = 40)]
    pub node_monitor_grace_period: u64,

    /// Period for syncing node status (seconds).
    #[arg(long, default_value_t = 5)]
    pub node_monitor_period: u64,

    /// Number of terminated pods that can exist before the pod GC deletes them (0 disables).
    #[arg(long, default_value_t = 12500)]
    pub terminated_pod_gc_threshold: usize,

    /// Cluster CA published to every namespace as the `kube-root-ca.crt` ConfigMap.
    #[arg(long)]
    pub root_ca_file: Option<std::path::PathBuf>,

    /// Should CIDRs for Pods be allocated and set on the cloud provider / nodes.
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set, num_args = 0..=1, default_missing_value = "true")]
    pub allocate_node_cidrs: bool,

    /// CIDR Range for Pods in cluster (comma separated, one per IP family). Requires --allocate-node-cidrs.
    #[arg(long, default_value = "")]
    pub cluster_cidr: String,

    /// Type of CIDR allocator to use.
    #[arg(long, default_value = "RangeAllocator")]
    pub cidr_allocator_type: String,

    /// Mask size for node cidr in cluster. Default is 24 for IPv4 and 64 for IPv6.
    #[arg(long, default_value_t = 0)]
    pub node_cidr_mask_size: u8,

    /// Mask size for IPv4 node cidr in dual-stack cluster.
    #[arg(long, default_value_t = 24)]
    pub node_cidr_mask_size_ipv4: u8,

    /// Mask size for IPv6 node cidr in dual-stack cluster.
    #[arg(long, default_value_t = 64)]
    pub node_cidr_mask_size_ipv6: u8,

    /// CIDR Range for Services in cluster (kept out of the pod CIDRs).
    #[arg(long, default_value = "")]
    pub service_cluster_ip_range: String,

    /// Secondary CIDR Range for Services in a dual-stack cluster.
    #[arg(long, default_value = "")]
    pub secondary_service_cluster_ip_range: String,

    /// Enable HostPath PV provisioning when running without a cloud provider. This allows testing
    /// and development of provisioning features. HostPath provisioning is not supported in any way,
    /// won't work in a multi-node cluster, and should not be used for anything other than testing
    /// or development.
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set, num_args = 0..=1, default_missing_value = "true")]
    pub enable_hostpath_provisioner: bool,

    /// Filename containing a PEM-encoded X509 CA certificate used to issue cluster-scoped certificates.
    /// If specified, no more specific --cluster-signing-* flag may be specified.
    #[arg(long, default_value = "")]
    pub cluster_signing_cert_file: String,
    /// Filename containing a PEM-encoded RSA or ECDSA private key used to sign cluster-scoped certificates.
    /// If specified, no more specific --cluster-signing-* flag may be specified.
    #[arg(long, default_value = "")]
    pub cluster_signing_key_file: String,
    /// Filename containing a PEM-encoded X509 CA certificate used to issue certificates for the
    /// kubernetes.io/kubelet-serving signer. If specified, --cluster-signing-{cert,key}-file must not be set.
    #[arg(long, default_value = "")]
    pub cluster_signing_kubelet_serving_cert_file: String,
    /// Filename containing a PEM-encoded private key used to sign certificates for the kubernetes.io/kubelet-serving signer.
    #[arg(long, default_value = "")]
    pub cluster_signing_kubelet_serving_key_file: String,
    /// Filename containing a PEM-encoded X509 CA certificate used to issue certificates for the
    /// kubernetes.io/kube-apiserver-client-kubelet signer.
    #[arg(long, default_value = "")]
    pub cluster_signing_kubelet_client_cert_file: String,
    /// Filename containing a PEM-encoded private key used to sign certificates for the kubernetes.io/kube-apiserver-client-kubelet signer.
    #[arg(long, default_value = "")]
    pub cluster_signing_kubelet_client_key_file: String,
    /// Filename containing a PEM-encoded X509 CA certificate used to issue certificates for the
    /// kubernetes.io/kube-apiserver-client signer.
    #[arg(long, default_value = "")]
    pub cluster_signing_kube_apiserver_client_cert_file: String,
    /// Filename containing a PEM-encoded private key used to sign certificates for the kubernetes.io/kube-apiserver-client signer.
    #[arg(long, default_value = "")]
    pub cluster_signing_kube_apiserver_client_key_file: String,
    /// Filename containing a PEM-encoded X509 CA certificate used to issue certificates for the
    /// kubernetes.io/legacy-unknown signer.
    #[arg(long, default_value = "")]
    pub cluster_signing_legacy_unknown_cert_file: String,
    /// Filename containing a PEM-encoded private key used to sign certificates for the kubernetes.io/legacy-unknown signer.
    #[arg(long, default_value = "")]
    pub cluster_signing_legacy_unknown_key_file: String,
    /// The max length of duration signed certificates will be given. Individual CSRs may request
    /// shorter certs by setting spec.expirationSeconds.
    #[arg(long, default_value = "8760h0m0s", value_parser = k8s_apimachinery::duration::parse_go_duration)]
    pub cluster_signing_duration: std::time::Duration,

    /// The period for syncing persistent volumes and persistent volume claims (seconds).
    #[arg(long, default_value_t = 15)]
    pub pvclaimbinder_sync_period: u64,

    /// Re-enqueue every object this often (seconds).
    #[arg(long, default_value_t = 30)]
    pub min_resync_period: u64,

    /// Address for the health/metrics server.
    #[arg(long, default_value = "127.0.0.1")]
    pub bind_address: String,

    /// Port for the health server (`/healthz`).
    #[arg(long, alias = "secure-port", default_value_t = 10257)]
    pub port: u16,

    /// Log level verbosity (0-1 info, 2-3 debug, 4+ trace). `RUST_LOG` overrides.
    #[arg(short = 'v', long = "v", default_value_t = 1)]
    pub v: u8,
}
