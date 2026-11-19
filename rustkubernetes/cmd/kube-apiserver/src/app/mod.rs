//! `cmd/kube-apiserver/app` (`server.go`): builds storage, the control plane
//! and serves until SIGINT/SIGTERM.

pub mod options;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use k8s_apiserver::authentication::{Authenticator, AuthenticatorChain, RejectUnknownBearer, TokenFileAuthenticator, X509Authenticator};
use k8s_apiserver::authorization::{AlwaysAllow, Attributes, Authorizer, AuthorizerChain, Decision, PrivilegedGroups};
use k8s_apiserver::{MemoryStore, Storage};
use k8s_client_go::util::cert::{self, TlsClientConfig};
use kube_controlplane::{bootstrap, new_server, Config, ServiceAccountConfig, KUBE_VERSION};
use kube_plugin::auth::node::NodeAuthorizer;
use kube_plugin::auth::rbac::RbacAuthorizer;
use kube_serviceaccount::{PublicKeys, TokenAuthenticator, TokenGenerator};

use options::ServerRunOptions;

/// `-v` maps to log levels like klog: 0-1 info, 2-3 debug, 4+ trace. `RUST_LOG` overrides.
pub fn init_logging(v: u8) {
    let level = match v {
        0 | 1 => "info",
        2 | 3 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).init();
}

pub async fn run(opts: ServerRunOptions) -> anyhow::Result<()> {
    tracing::info!(version = KUBE_VERSION, "starting kube-apiserver (rustkubernetes)");
    let storage = open_storage(&opts).await?;
    let mut cfg = Config {
        service_cidr: opts.service_cluster_ip_range.clone(),
        node_port_range: opts.service_node_port_range.clone(),
        advertise_address: opts.advertise_address.clone(),
        advertise_port: opts.port,
        enable_admission_plugins: opts.enable_admission_plugins.clone(),
        disable_admission_plugins: opts.disable_admission_plugins.clone(),
        ..Default::default()
    };
    let secure = opts.tls_cert_file.is_some();
    if secure {
        configure_security(&opts, &mut cfg, storage.clone()).await?;
    } else {
        tracing::warn!("no --tls-cert-file: serving insecure HTTP, every request is treated as system:admin (development only)");
    }
    let server = new_server(&cfg, storage.clone()).map_err(|e| anyhow::anyhow!(e))?;
    tracing::info!(plugins = ?server.admission.names(), "admission plugins enabled");
    bootstrap(&server, &cfg).await.map_err(|e| anyhow::anyhow!(e.message))?;
    publish_authentication_info(&server, &opts).await?;
    let read_opt = |p: &Option<std::path::PathBuf>| -> anyhow::Result<Option<Vec<u8>>> {
        p.as_ref().map(|p| std::fs::read(p).with_context(|| format!("reading {}", p.display()))).transpose()
    };
    let agg = kube_aggregator::Config {
        proxy_client_cert: read_opt(&opts.proxy_client_cert_file)?,
        proxy_client_key: read_opt(&opts.proxy_client_key_file)?,
        enable_aggregator_routing: opts.enable_aggregator_routing,
    };
    if agg.proxy_client_cert.is_none() {
        tracing::warn!("no --proxy-client-cert-file: extension API servers cannot authenticate this server");
    }
    // apiextensions-apiserver: serve established CustomResourceDefinitions.
    k8s_apiextensions_apiserver::start(server.clone()).await;
    kube_aggregator::KubeAggregator::start(server.clone(), agg).await;

    let addr: SocketAddr = format!("{}:{}", opts.bind_address, opts.port).parse().context("invalid --bind-address/--port")?;
    if secure {
        let read = |p: &Option<std::path::PathBuf>| -> anyhow::Result<Vec<u8>> {
            let p = p.as_ref().context("--tls-private-key-file is required with --tls-cert-file")?;
            std::fs::read(p).with_context(|| format!("reading {}", p.display()))
        };
        let ca = opts.client_ca_file.as_ref().map(std::fs::read).transpose().context("reading --client-ca-file")?;
        let tls = cert::server_config(&read(&opts.tls_cert_file)?, &read(&opts.tls_private_key_file)?, ca.as_deref())
            .map_err(anyhow::Error::msg)?;
        let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
        server.clone().serve_tls(listener, tls, shutdown_signal()).await?;
    } else {
        server.serve(addr, shutdown_signal()).await?;
    }
    if let Err(e) = storage.flush() {
        tracing::error!("final snapshot failed: {e}");
    }
    tracing::info!("kube-apiserver stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
        tokio::select! {
            _ = ctrl_c => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
    tracing::info!("shutdown signal received, draining connections");
}

/// `AlwaysDeny`
struct AlwaysDeny;

#[async_trait::async_trait]
impl Authorizer for AlwaysDeny {
    async fn authorize(&self, _a: &Attributes) -> Decision {
        Decision::Deny("Everything is forbidden.".into())
    }
}

/// Builds authentication, authorization, ServiceAccount tokens and kubelet TLS
/// (`BuildAuthenticator`, `BuildAuthorizer`, `ServiceAccountTokenGetter`).
async fn configure_security(opts: &ServerRunOptions, cfg: &mut Config, storage: Arc<dyn Storage>) -> anyhow::Result<()> {
    cert::install_crypto_provider();
    let read = |p: &std::path::Path| std::fs::read(p).with_context(|| format!("reading {}", p.display()));

    // Authentication: x509 → static tokens → ServiceAccount JWTs → reject unknown tokens → anonymous.
    let mut authenticators: Vec<Arc<dyn Authenticator>> = vec![];
    let mut kinds = vec![];
    if opts.client_ca_file.is_some() {
        authenticators.push(Arc::new(X509Authenticator));
        kinds.push("x509");
    }
    if let Some(p) = &opts.token_auth_file {
        let csv = String::from_utf8(read(p)?)?;
        authenticators.push(Arc::new(TokenFileAuthenticator::parse(&csv).map_err(anyhow::Error::msg)?));
        kinds.push("token-file");
    }
    let audiences = if opts.api_audiences.is_empty() { vec![opts.service_account_issuer[0].clone()] } else { opts.api_audiences.clone() };
    let mut pub_pem = vec![];
    for p in &opts.service_account_key_file {
        pub_pem.extend(read(p)?);
        pub_pem.push(b'\n');
    }
    if let Some(signing) = &opts.service_account_signing_key_file {
        if pub_pem.is_empty() {
            anyhow::bail!("--service-account-key-file is required with --service-account-signing-key-file");
        }
        let first_pub = read(&opts.service_account_key_file[0])?;
        let generator = TokenGenerator::new(&opts.service_account_issuer[0], &read(signing)?, &first_pub).map_err(anyhow::Error::msg)?;
        cfg.service_accounts = Some(ServiceAccountConfig {
            generator: Arc::new(generator),
            audiences: audiences.clone(),
            max_expiration: opts.service_account_max_token_expiration,
        });
    }
    if !pub_pem.is_empty() {
        let keys = PublicKeys::from_pem(&pub_pem).map_err(anyhow::Error::msg)?;
        let issuer = opts.service_account_issuer[0].clone();
        cfg.extra_paths.push(("/.well-known/openid-configuration".into(), kube_serviceaccount::openid_configuration(&issuer)));
        cfg.extra_paths.push(("/openid/v1/jwks".into(), keys.jwks()));
        authenticators.push(Arc::new(TokenAuthenticator {
            issuers: opts.service_account_issuer.clone(),
            audiences,
            keys,
            storage: storage.clone(),
        }));
        kinds.push("serviceaccount");
    }
    if opts.enable_bootstrap_token_auth {
        authenticators.push(Arc::new(kube_plugin::auth::bootstrap::BootstrapTokenAuthenticator { storage: storage.clone() }));
        kinds.push("bootstrap-token");
    }
    authenticators.push(Arc::new(RejectUnknownBearer));
    if opts.anonymous_auth {
        kinds.push("anonymous");
    }
    cfg.authenticator = Some(Arc::new(AuthenticatorChain { authenticators, anonymous: opts.anonymous_auth }));

    // Authorization.
    let modes =
        if opts.authorization_mode.is_empty() { vec!["Node".to_string(), "RBAC".to_string()] } else { opts.authorization_mode.clone() };
    let mut chain: Vec<Arc<dyn Authorizer>> = vec![Arc::new(PrivilegedGroups)];
    for m in &modes {
        match m.as_str() {
            "RBAC" => chain.push(RbacAuthorizer::start(storage.clone()).await),
            "AlwaysAllow" => chain.push(Arc::new(AlwaysAllow)),
            "AlwaysDeny" => chain.push(Arc::new(AlwaysDeny)),
            "Node" => chain.push(NodeAuthorizer::start(storage.clone()).await),
            other => anyhow::bail!("unknown --authorization-mode {other:?} (want RBAC, AlwaysAllow, AlwaysDeny, Node)"),
        }
    }
    cfg.authorizer = Some(Arc::new(AuthorizerChain(chain)));

    // API server → kubelet.
    if let (Some(c), Some(k)) = (&opts.kubelet_client_certificate, &opts.kubelet_client_key) {
        let tls = TlsClientConfig {
            ca_data: opts.kubelet_certificate_authority.as_deref().map(read).transpose()?,
            cert_data: Some(read(c)?),
            key_data: Some(read(k)?),
            insecure: opts.kubelet_certificate_authority.is_none(),
            cert_resolver: None,
        };
        cfg.kubelet_tls = Some(Arc::new(tls.rustls().map_err(anyhow::Error::msg)?));
    }
    tracing::info!(authentication = ?kinds, authorization = ?modes, service_account_tokens = cfg.service_accounts.is_some(), kubelet_tls = cfg.kubelet_tls.is_some(), "secure serving enabled");
    Ok(())
}

/// The cluster-authentication-trust controller: publishes how this server authenticates
/// clients and front proxies in `kube-system/extension-apiserver-authentication`, where
/// extension API servers (metrics-server) read it.
async fn publish_authentication_info(server: &std::sync::Arc<k8s_apiserver::ApiServer>, opts: &ServerRunOptions) -> anyhow::Result<()> {
    let mut data = serde_json::Map::new();
    if let Some(p) = &opts.client_ca_file {
        data.insert("client-ca-file".into(), serde_json::json!(std::fs::read_to_string(p).context("reading --client-ca-file")?));
    }
    if let Some(p) = &opts.requestheader_client_ca_file {
        let list = |v: &[String]| serde_json::to_string(&v.iter().filter(|x| !x.is_empty()).collect::<Vec<_>>()).unwrap();
        data.insert("requestheader-username-headers".into(), serde_json::json!(list(&opts.requestheader_username_headers)));
        data.insert("requestheader-group-headers".into(), serde_json::json!(list(&opts.requestheader_group_headers)));
        data.insert("requestheader-extra-headers-prefix".into(), serde_json::json!(list(&opts.requestheader_extra_headers_prefix)));
        data.insert(
            "requestheader-client-ca-file".into(),
            serde_json::json!(std::fs::read_to_string(p).context("reading --requestheader-client-ca-file")?),
        );
        data.insert("requestheader-allowed-names".into(), serde_json::json!(list(&opts.requestheader_allowed_names)));
    }
    if data.is_empty() {
        return Ok(());
    }
    let Some(info) = server.registry.lookup("", "v1", "configmaps") else { return Ok(()) };
    let key = info.key("kube-system", "extension-apiserver-authentication");
    match server.storage.get(&key).await {
        Ok(mut cur) => {
            if cur.get("data") != Some(&serde_json::Value::Object(data.clone())) {
                cur["data"] = serde_json::Value::Object(data);
                server.storage.update(&key, None, cur).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            }
        }
        Err(_) => {
            let obj = serde_json::json!({"apiVersion": "v1", "kind": "ConfigMap",
                "metadata": {"name": "extension-apiserver-authentication", "namespace": "kube-system"}, "data": data});
            server.create_internal("", "v1", "configmaps", "kube-system", obj).await.map_err(|e| anyhow::anyhow!(e.message))?;
        }
    }
    Ok(())
}

/// `--etcd-servers` → etcd3 storage (as upstream); otherwise the in-memory store.
async fn open_storage(opts: &ServerRunOptions) -> anyhow::Result<Arc<dyn Storage>> {
    if opts.storage_media_type != "application/json" {
        anyhow::bail!("--storage-media-type {:?} is not supported: use application/json", opts.storage_media_type);
    }
    let transformer = match &opts.encryption_provider_config {
        Some(p) => {
            let yaml = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
            Some(Arc::new(k8s_apiserver::storage::value::Transformer::from_yaml(&yaml).map_err(anyhow::Error::msg)?))
        }
        None => None,
    };
    if opts.etcd_servers.is_empty() {
        if transformer.is_some() {
            anyhow::bail!("--encryption-provider-config needs --etcd-servers");
        }
        return Ok(match &opts.data_dir {
            Some(dir) => MemoryStore::open(dir).map_err(|e| anyhow::anyhow!(e)).context("opening storage")?,
            None => {
                tracing::warn!("no --etcd-servers or --data-dir given: cluster state is kept in memory only");
                MemoryStore::new()
            }
        });
    }
    let read = |p: &Option<std::path::PathBuf>| -> anyhow::Result<Option<Vec<u8>>> {
        p.as_ref().map(|p| std::fs::read(p).with_context(|| format!("reading {}", p.display()))).transpose()
    };
    let cfg = k8s_apiserver::storage::etcd3::EtcdConfig {
        endpoints: opts.etcd_servers.clone(),
        ca: read(&opts.etcd_cafile)?,
        cert: read(&opts.etcd_certfile)?,
        key: read(&opts.etcd_keyfile)?,
        prefix: opts.etcd_prefix.clone(),
        compaction_interval: (!opts.etcd_compaction_interval.is_zero()).then_some(opts.etcd_compaction_interval),
        transformer,
    };
    let store = k8s_apiserver::storage::etcd3::EtcdStore::connect(cfg).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(store)
}
