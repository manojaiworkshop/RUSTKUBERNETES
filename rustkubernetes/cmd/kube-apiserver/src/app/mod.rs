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
    let storage: Arc<dyn Storage> = match &opts.data_dir {
        Some(dir) => MemoryStore::open(dir).map_err(|e| anyhow::anyhow!(e)).context("opening storage")?,
        None => {
            tracing::warn!("no --data-dir given: cluster state is kept in memory only");
            MemoryStore::new()
        }
    };
    let mut cfg = Config {
        service_cidr: opts.service_cluster_ip_range.clone(),
        node_port_range: opts.service_node_port_range.clone(),
        advertise_address: opts.advertise_address.clone(),
        advertise_port: opts.port,
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
            "Node" => tracing::info!("authorization mode Node: node permissions come from the system:node ClusterRole (RBAC)"),
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
        };
        cfg.kubelet_tls = Some(Arc::new(tls.rustls().map_err(anyhow::Error::msg)?));
    }
    tracing::info!(authentication = ?kinds, authorization = ?modes, service_account_tokens = cfg.service_accounts.is_some(), kubelet_tls = cfg.kubelet_tls.is_some(), "secure serving enabled");
    Ok(())
}
