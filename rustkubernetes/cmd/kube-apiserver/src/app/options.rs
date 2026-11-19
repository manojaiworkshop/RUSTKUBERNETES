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

    /// Root certificate bundle to verify client certificates on incoming requests before trusting
    /// usernames in headers specified by --requestheader-username-headers (front proxy / aggregator).
    #[arg(long)]
    pub requestheader_client_ca_file: Option<PathBuf>,

    /// List of client certificate common names to allow to provide usernames in headers.
    #[arg(long, value_delimiter = ',')]
    pub requestheader_allowed_names: Vec<String>,

    /// List of request headers to inspect for usernames. X-Remote-User is common.
    #[arg(long, value_delimiter = ',')]
    pub requestheader_username_headers: Vec<String>,

    /// List of request headers to inspect for groups. X-Remote-Group is suggested.
    #[arg(long, value_delimiter = ',')]
    pub requestheader_group_headers: Vec<String>,

    /// List of request header prefixes to inspect. X-Remote-Extra- is suggested.
    #[arg(long, value_delimiter = ',')]
    pub requestheader_extra_headers_prefix: Vec<String>,

    /// Client certificate used to prove the identity of the aggregator or kube-apiserver when it
    /// must call out during a request (proxying to extension API servers).
    #[arg(long)]
    pub proxy_client_cert_file: Option<PathBuf>,

    /// Private key for the client certificate used by the aggregator.
    #[arg(long)]
    pub proxy_client_key_file: Option<PathBuf>,

    /// Turns on aggregator routing requests to endpoints IP rather than cluster IP.
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set, num_args = 0..=1, default_missing_value = "true")]
    pub enable_aggregator_routing: bool,

    /// Enable to allow secrets of type 'bootstrap.kubernetes.io/token' in the 'kube-system'
    /// namespace to be used for TLS bootstrapping authentication.
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set, num_args = 0..=1, default_missing_value = "true")]
    pub enable_bootstrap_token_auth: bool,

    /// Static bearer tokens: CSV `token,user,uid,"group1,group2"`.
    #[arg(long)]
    pub token_auth_file: Option<PathBuf>,

    /// Allow unauthenticated requests as `system:anonymous`.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub anonymous_auth: bool,

    /// admission plugins that should be enabled in addition to default enabled ones
    /// (NamespaceLifecycle, ServiceAccount, Priority, DefaultTolerationSeconds, DefaultStorageClass,
    /// StorageObjectInUseProtection). Also available: NodeRestriction.
    #[arg(long, value_delimiter = ',')]
    pub enable_admission_plugins: Vec<String>,

    /// admission plugins that should be disabled although they are in the default enabled plugins list.
    #[arg(long, value_delimiter = ',')]
    pub disable_admission_plugins: Vec<String>,

    /// Ordered authorizers: `Node`, `RBAC`, `AlwaysAllow`, `AlwaysDeny`.
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

    /// etcd servers to connect to, comma separated (`https://127.0.0.1:2379`).
    /// Without it, state lives in memory (persisted to --data-dir if given).
    #[arg(long, value_delimiter = ',')]
    pub etcd_servers: Vec<String>,

    /// SSL Certificate Authority file used to secure etcd communication.
    #[arg(long)]
    pub etcd_cafile: Option<PathBuf>,

    /// SSL certification file used to secure etcd communication.
    #[arg(long)]
    pub etcd_certfile: Option<PathBuf>,

    /// SSL key file used to secure etcd communication.
    #[arg(long)]
    pub etcd_keyfile: Option<PathBuf>,

    /// The prefix to prepend to all resource paths in etcd.
    #[arg(long, default_value = "/registry")]
    pub etcd_prefix: String,

    /// The interval of compaction requests (0 disables compaction from the API server).
    #[arg(long, default_value = "5m", value_parser = parse_duration)]
    pub etcd_compaction_interval: std::time::Duration,

    /// The media type to use to store objects in storage. Only application/json is implemented
    /// (upstream's default is application/vnd.kubernetes.protobuf).
    #[arg(long, default_value = "application/json")]
    pub storage_media_type: String,

    /// File with an EncryptionConfiguration: how resources are encrypted at rest in etcd.
    #[arg(long)]
    pub encryption_provider_config: Option<PathBuf>,

    /// Directory for persisted cluster state when no etcd is used. Omit for in-memory only.
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

/// `5m`, `30s`, `1h`, `0`.
pub fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    let (num, unit) = s.find(|c: char| !c.is_ascii_digit()).map(|i| s.split_at(i)).unwrap_or((s, "s"));
    let n: u64 = num.parse().map_err(|_| format!("invalid duration {s:?}"))?;
    let secs = match unit {
        "s" | "" => n,
        "m" => n * 60,
        "h" => n * 3600,
        _ => return Err(format!("invalid duration unit in {s:?}")),
    };
    Ok(std::time::Duration::from_secs(secs))
}
