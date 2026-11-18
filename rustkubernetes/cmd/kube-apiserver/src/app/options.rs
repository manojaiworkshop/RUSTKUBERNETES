//! `cmd/kube-apiserver/app/options`: command-line flags.

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "kube-apiserver",
    version,
    about = "The Kubernetes API server validates and configures data for the api objects which include pods, services, replicationcontrollers, and others."
)]
pub struct ServerRunOptions {
    /// The IP address on which to listen.
    #[arg(long, default_value = "0.0.0.0")]
    pub bind_address: String,

    /// The port to serve on: HTTPS with `--tls-cert-file`, otherwise insecure HTTP (development only).
    #[arg(long, alias = "secure-port", default_value_t = 6443)]
    pub port: u16,

    /// Serving certificate (PEM). Enables HTTPS, authentication and authorization.
    #[arg(long)]
    pub tls_cert_file: Option<PathBuf>,
    #[arg(long)]
    pub tls_private_key_file: Option<PathBuf>,

    /// CA bundle to verify client certificates (x509 authentication: CN = user, O = groups).
    #[arg(long)]
    pub client_ca_file: Option<PathBuf>,

    /// Static bearer tokens: CSV `token,user,uid,"group1,group2"`.
    #[arg(long)]
    pub token_auth_file: Option<PathBuf>,

    /// Allow unauthenticated requests as `system:anonymous`.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub anonymous_auth: bool,

    /// Ordered authorizers: `RBAC`, `AlwaysAllow`, `AlwaysDeny` (`Node` is accepted and served by RBAC).
    /// Default: `Node,RBAC` with TLS, `AlwaysAllow` without.
    #[arg(long, value_delimiter = ',')]
    pub authorization_mode: Vec<String>,

    /// Public keys that verify ServiceAccount tokens (PEM, may repeat).
    #[arg(long)]
    pub service_account_key_file: Vec<PathBuf>,

    /// Private key that signs ServiceAccount tokens (PKCS#8 ECDSA P-256).
    #[arg(long)]
    pub service_account_signing_key_file: Option<PathBuf>,

    /// `iss` of issued ServiceAccount tokens (repeat to also accept older issuers).
    #[arg(long, default_values_t = vec!["https://kubernetes.default.svc.cluster.local".to_string()])]
    pub service_account_issuer: Vec<String>,

    /// Accepted token audiences (default: the first issuer).
    #[arg(long, value_delimiter = ',')]
    pub api_audiences: Vec<String>,

    /// Maximum lifetime of issued tokens, in seconds.
    #[arg(long)]
    pub service_account_max_token_expiration: Option<i64>,

    /// Client certificate/key the API server uses towards kubelets (logs, exec). Enables HTTPS to kubelets.
    #[arg(long)]
    pub kubelet_client_certificate: Option<PathBuf>,
    #[arg(long)]
    pub kubelet_client_key: Option<PathBuf>,

    /// CA bundle to verify kubelet serving certificates (unverified if unset, like upstream).
    #[arg(long)]
    pub kubelet_certificate_authority: Option<PathBuf>,

    /// The IP address on which to advertise the apiserver to members of the cluster.
    #[arg(long, default_value = "127.0.0.1")]
    pub advertise_address: String,

    /// Directory for persisted cluster state (the etcd replacement). Omit for in-memory only.
    #[arg(long, env = "KUBE_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// A CIDR notation IP range from which to assign service cluster IPs.
    #[arg(long, default_value = "10.96.0.0/12")]
    pub service_cluster_ip_range: String,

    /// A port range to reserve for services with NodePort visibility.
    #[arg(long, default_value = "30000-32767")]
    pub service_node_port_range: String,

    /// Log level verbosity (0-1 info, 2-3 debug, 4+ trace). `RUST_LOG` overrides.
    #[arg(short = 'v', long = "v", default_value_t = 1)]
    pub v: u8,
}
