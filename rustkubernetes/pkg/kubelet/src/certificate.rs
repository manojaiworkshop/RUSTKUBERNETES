//! Kubelet certificates: port of `k8s.io/client-go/util/certificate` (file
//! store and rotating manager), `pkg/kubelet/certificate` (client and serving
//! managers) and `pkg/kubelet/certificate/bootstrap` (TLS bootstrapping).
//!
//! * `--bootstrap-kubeconfig`: when `--kubeconfig` is missing or its certificate
//!   expired, request a client certificate with the bootstrap credentials and
//!   write a kubeconfig pointing at `<cert-dir>/kubelet-client-current.pem`.
//! * `--rotate-certificates`: request a new client certificate when 70–84 % of
//!   the current one's lifetime has passed (signer `kube-apiserver-client-kubelet`).
//! * `--rotate-server-certificates`: the serving certificate is requested the
//!   same way (signer `kubelet-serving`, SANs = node hostname and IP).
//!
//! The store keeps `<prefix>-<timestamp>.pem` files (certificate + key) and a
//! `<prefix>-current.pem` symlink to the newest.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context};
use base64::Engine;
use k8s_api::certificates::v1::*;
use k8s_apimachinery::meta::ObjectMeta;
use k8s_client_go::util::cert::DynamicCert;
use k8s_client_go::{Api, Client};
use x509_parser::pem::Pem;

/// `certificate.FileStore`
#[derive(Clone, Debug)]
pub struct FileStore {
    pub dir: PathBuf,
    pub prefix: String,
    /// Used while the store is empty (`--tls-cert-file`, the kubeconfig's certificate).
    pub fallback: Option<(Vec<u8>, Vec<u8>)>,
}

impl FileStore {
    pub fn new(dir: &Path, prefix: &str) -> Self {
        Self { dir: dir.to_path_buf(), prefix: prefix.into(), fallback: None }
    }

    /// `CurrentPath`
    pub fn current_path(&self) -> PathBuf {
        self.dir.join(format!("{}-current.pem", self.prefix))
    }

    /// `Current`: (certificate PEM, key PEM).
    pub fn current(&self) -> Option<(Vec<u8>, Vec<u8>)> {
        if let Ok(data) = std::fs::read(self.current_path()) {
            if let Some(pair) = split_pair(&data) {
                return Some(pair);
            }
        }
        self.fallback.clone()
    }

    /// `Update`: writes `<prefix>-<timestamp>.pem` and repoints the `-current` symlink.
    pub fn update(&self, cert_pem: &[u8], key_pem: &[u8]) -> anyhow::Result<PathBuf> {
        std::fs::create_dir_all(&self.dir).with_context(|| format!("could not create directory {:?}", self.dir))?;
        let ts = chrono::Utc::now().format("%Y-%m-%d-%H-%M-%S");
        let file = self.dir.join(format!("{}-{ts}.pem", self.prefix));
        let mut data = cert_pem.to_vec();
        if !data.ends_with(b"\n") {
            data.push(b'\n');
        }
        data.extend_from_slice(key_pem);
        write_private(&file, &data)?;
        let tmp = self.dir.join(format!("{}-updated.pem", self.prefix));
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(&file, &tmp)?;
        std::fs::rename(&tmp, self.current_path())?;
        Ok(file)
    }
}

fn write_private(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    f.write_all(data)?;
    Ok(())
}

/// Splits a store file into its certificate blocks and its key block.
fn split_pair(data: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let (mut certs, mut key) = (String::new(), String::new());
    for b in Pem::iter_from_buffer(data) {
        let b = b.ok()?;
        let block = pem_block(&b.label, &b.contents);
        if b.label == "CERTIFICATE" {
            certs.push_str(&block);
        } else if b.label.ends_with("PRIVATE KEY") {
            key = block;
        }
    }
    (!certs.is_empty() && !key.is_empty()).then(|| (certs.into_bytes(), key.into_bytes()))
}

fn pem_block(label: &str, der: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for c in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(c).unwrap());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// (notBefore, notAfter) of the first certificate, unix seconds.
pub fn validity(cert_pem: &[u8]) -> Option<(i64, i64)> {
    let b = Pem::iter_from_buffer(cert_pem).next()?.ok()?;
    let c = b.parse_x509().ok()?;
    Some((c.validity().not_before.timestamp(), c.validity().not_after.timestamp()))
}

/// What to request (`x509.CertificateRequest` template + usages + signer).
#[derive(Clone, Debug)]
pub struct Template {
    pub common_name: String,
    pub organizations: Vec<String>,
    pub dns_names: Vec<String>,
    pub ip_addresses: Vec<IpAddr>,
    pub usages: Vec<String>,
    pub signer_name: &'static str,
}

impl Template {
    /// Kubelet client certificate (`O=system:nodes, CN=system:node:<name>`).
    /// ECDSA keys: no key encipherment.
    pub fn kubelet_client(node: &str) -> Self {
        Self {
            common_name: format!("system:node:{node}"),
            organizations: vec!["system:nodes".into()],
            dns_names: vec![],
            ip_addresses: vec![],
            usages: vec!["digital signature".into(), "client auth".into()],
            signer_name: KUBE_APISERVER_CLIENT_KUBELET_SIGNER_NAME,
        }
    }

    /// Kubelet serving certificate for the node's hostname and IPs.
    pub fn kubelet_serving(node: &str, hostnames: &[String], ips: &[IpAddr]) -> Self {
        Self {
            common_name: format!("system:node:{node}"),
            organizations: vec!["system:nodes".into()],
            dns_names: hostnames.to_vec(),
            ip_addresses: ips.to_vec(),
            usages: vec!["digital signature".into(), "server auth".into()],
            signer_name: KUBELET_SERVING_SIGNER_NAME,
        }
    }

    /// A PEM `CERTIFICATE REQUEST` for `key` (`cert.MakeCSRFromTemplate`).
    pub fn csr_pem(&self, key: &rcgen::KeyPair) -> anyhow::Result<String> {
        let mut p = rcgen::CertificateParams::default();
        let mut dn = rcgen::DistinguishedName::new();
        for o in &self.organizations {
            dn.push(rcgen::DnType::OrganizationName, o.as_str());
        }
        dn.push(rcgen::DnType::CommonName, self.common_name.as_str());
        p.distinguished_name = dn;
        for d in &self.dns_names {
            p.subject_alt_names.push(rcgen::SanType::DnsName(d.as_str().try_into()?));
        }
        for ip in &self.ip_addresses {
            p.subject_alt_names.push(rcgen::SanType::IpAddress(*ip));
        }
        Ok(p.serialize_request(key)?.pem()?)
    }
}

/// `digestedName`: the bootstrap request's deterministic name, so a restarted
/// kubelet finds its own pending request.
pub fn digested_name(public_key_der: &[u8], t: &Template) -> String {
    let b64 = |d: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(d);
    let mut data = Vec::new();
    let mut encode = |d: &[u8]| {
        data.extend_from_slice(b64(d).as_bytes());
        data.push(b'|');
    };
    encode(public_key_der);
    encode(t.common_name.as_bytes());
    for o in &t.organizations {
        encode(o.as_bytes());
    }
    for u in &t.usages {
        encode(u.as_bytes());
    }
    let digest = ring::digest::digest(&ring::digest::SHA512_256, &data);
    format!("node-csr-{}", b64(digest.as_ref()))
}

/// `RequestCertificate` + `WaitForCertificate`: creates the request (or finds
/// the one with the same name) and waits until it is issued, denied or failed.
pub async fn request_certificate(
    client: &Client,
    name: Option<&str>,
    t: &Template,
    key: &rcgen::KeyPair,
    timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    let api: Api<CertificateSigningRequest> = Api::all(client.clone());
    let csr = CertificateSigningRequest {
        metadata: ObjectMeta {
            name: name.map(str::to_string),
            generate_name: name.is_none().then(|| "csr-".to_string()),
            ..Default::default()
        },
        spec: CertificateSigningRequestSpec {
            request: base64::engine::general_purpose::STANDARD.encode(t.csr_pem(key)?),
            signer_name: t.signer_name.into(),
            usages: Some(t.usages.clone()),
            ..Default::default()
        },
        ..CertificateSigningRequest::new(Default::default())
    };
    let created = match api.create(&csr).await {
        Ok(c) => c,
        Err(e) if e.is_already_exists() && name.is_some() => {
            // `ensureCompatible`: reuse our own earlier request.
            let existing = api.get(name.unwrap()).await?;
            if existing.spec.request != csr.spec.request || existing.spec.signer_name != csr.spec.signer_name {
                bail!("retrieved csr is not compatible");
            }
            existing
        }
        Err(e) => return Err(e).context("cannot create certificate signing request"),
    };
    let req_name = created.metadata.name.clone().unwrap_or_default();
    let uid = created.metadata.uid.clone();
    tracing::info!(csr = %req_name, signer = t.signer_name, "waiting for client certificate to be issued");
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let cur = api.get(&req_name).await?;
        if cur.metadata.uid != uid {
            bail!("csr {req_name:?} changed UIDs");
        }
        for c in cur.conditions() {
            let (reason, message) = (c.reason.clone().unwrap_or_default(), c.message.clone().unwrap_or_default());
            if c.type_ == CERTIFICATE_DENIED {
                bail!("certificate signing request is denied, reason: {reason}, message: {message}");
            }
            if c.type_ == CERTIFICATE_FAILED {
                bail!("certificate signing request failed, reason: {reason}, message: {message}");
            }
        }
        if cur.is_approved() {
            if let Some(cert) = cur.certificate() {
                tracing::info!(csr = %req_name, "certificate signing request is issued");
                return Ok(base64::engine::general_purpose::STANDARD.decode(cert)?);
            }
        }
        if tokio::time::Instant::now() > deadline {
            bail!("timed out waiting for the condition");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// `certificate.Manager`: keeps a certificate in `cert` and renews it in time.
pub struct Manager {
    name: &'static str,
    client: Client,
    store: FileStore,
    template: Template,
    pub cert: Arc<DynamicCert>,
    validity: Mutex<Option<(i64, i64)>>,
    /// Client certificates: exit when the current one expired and cannot be renewed.
    exit_on_expiry: bool,
}

impl Manager {
    /// Loads the current certificate from the store (or its fallback) into `cert`.
    pub fn new(
        name: &'static str,
        client: Client,
        store: FileStore,
        template: Template,
        cert: Arc<DynamicCert>,
        exit_on_expiry: bool,
    ) -> anyhow::Result<Arc<Self>> {
        let m = Arc::new(Self { name, client, store, template, cert, validity: Mutex::new(None), exit_on_expiry });
        if let Some((c, k)) = m.store.current() {
            m.set(&c, &k)?;
        }
        Ok(m)
    }

    fn set(&self, cert: &[u8], key: &[u8]) -> anyhow::Result<()> {
        self.cert.set(cert, key).map_err(anyhow::Error::msg)?;
        *self.validity.lock().unwrap() = validity(cert);
        Ok(())
    }

    /// `nextRotationDeadline`: notBefore + lifetime × jitter(0.7, 0.2).
    fn next_rotation_deadline(&self) -> i64 {
        let Some((nb, na)) = *self.validity.lock().unwrap() else { return 0 };
        let total = (na - nb) as f64;
        let mut b = [0u8; 4];
        let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b);
        let r = u32::from_le_bytes(b) as f64 / u32::MAX as f64;
        let jittered = total * 0.7 * (1.0 + 0.2 * r);
        nb + jittered as i64
    }

    /// `rotateCerts`
    async fn rotate(&self) -> anyhow::Result<()> {
        tracing::info!(manager = self.name, "rotating certificates");
        let key = rcgen::KeyPair::generate()?;
        let cert = request_certificate(&self.client, None, &self.template, &key, Duration::from_secs(15 * 60)).await?;
        let key_pem = key.serialize_pem();
        self.store.update(&cert, key_pem.as_bytes())?;
        self.set(&cert, key_pem.as_bytes())?;
        if let Some((_, na)) = *self.validity.lock().unwrap() {
            tracing::info!(manager = self.name, expiration = %chrono::DateTime::from_timestamp(na, 0).unwrap_or_default(), "certificate rotated");
        }
        Ok(())
    }

    /// `Start`: wait for the rotation deadline, rotate, repeat (backoff on failure).
    pub async fn run(self: Arc<Self>) {
        let mut backoff = Duration::from_secs(2);
        loop {
            let deadline = self.next_rotation_deadline();
            let now = chrono::Utc::now().timestamp();
            if deadline > now {
                if let Some((_, na)) = *self.validity.lock().unwrap() {
                    tracing::info!(
                        manager = self.name,
                        expiration = %chrono::DateTime::from_timestamp(na, 0).unwrap_or_default(),
                        deadline = %chrono::DateTime::from_timestamp(deadline, 0).unwrap_or_default(),
                        "certificate rotation deadline determined"
                    );
                }
                tokio::time::sleep(Duration::from_secs((deadline - now) as u64)).await;
            }
            match self.rotate().await {
                Ok(()) => backoff = Duration::from_secs(2),
                Err(e) => {
                    tracing::warn!(manager = self.name, "failed while requesting a signed certificate from the control plane: {e:#}");
                    let expired = self.validity.lock().unwrap().is_some_and(|(_, na)| chrono::Utc::now().timestamp() > na);
                    if expired && self.exit_on_expiry {
                        tracing::error!("The currently active client certificate has expired and the server is responsive, exiting.");
                        std::process::exit(1);
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(128));
                }
            }
        }
    }
}

/// `isClientConfigStillValid`: the kubeconfig exists and its certificate has not expired.
fn client_config_still_valid(kubeconfig: &Path) -> bool {
    if !kubeconfig.exists() {
        return false;
    }
    let cfg = match k8s_client_go::tools::clientcmd::load_config(kubeconfig, None) {
        Ok(c) => c,
        Err(e) => {
            tracing::info!(path = %kubeconfig.display(), "unable to read existing bootstrap client config: {e}");
            return false;
        }
    };
    let Some(cert) = cfg.tls.cert_data.as_deref() else { return cfg.bearer_token.is_some() };
    match validity(cert) {
        Some((_, na)) if chrono::Utc::now().timestamp() > na => {
            tracing::info!(path = %kubeconfig.display(), "Part of the existing bootstrap client certificate is expired");
            false
        }
        Some(_) => true,
        None => false,
    }
}

/// `LoadClientCert` (TLS bootstrapping).
pub async fn load_client_cert(kubeconfig: &Path, bootstrap: &Path, cert_dir: &Path, node: &str) -> anyhow::Result<()> {
    if client_config_still_valid(kubeconfig) {
        tracing::debug!(path = %kubeconfig.display(), "Kubeconfig exists and is valid, skipping bootstrap");
        return Ok(());
    }
    tracing::info!("Using bootstrap kubeconfig to generate TLS client cert, key and kubeconfig file");
    let boot_cfg = k8s_client_go::tools::clientcmd::load_config(bootstrap, None)
        .map_err(|e| anyhow::anyhow!("unable to load bootstrap kubeconfig: {e}"))?;
    let boot_client = Client::new(boot_cfg.clone())?;
    let store = FileStore::new(cert_dir, "kubelet-client");
    std::fs::create_dir_all(cert_dir)?;
    // Reuse the key across restarts until a certificate is issued.
    let tmp_key = cert_dir.join("kubelet-client.key.tmp");
    let key = match std::fs::read_to_string(&tmp_key).ok().and_then(|p| rcgen::KeyPair::from_pem(&p).ok()) {
        Some(k) => k,
        None => {
            tracing::info!("No valid private key and/or certificate found, reusing existing private key or creating a new one");
            let k = rcgen::KeyPair::generate()?;
            write_private(&tmp_key, k.serialize_pem().as_bytes())?;
            k
        }
    };
    let t = Template::kubelet_client(node);
    let name = digested_name(&key.public_key_der(), &t);
    let cert = request_certificate(&boot_client, Some(&name), &t, &key, Duration::from_secs(3600)).await?;
    store.update(&cert, key.serialize_pem().as_bytes())?;
    let _ = std::fs::remove_file(&tmp_key);
    write_kubeconfig_from_bootstrapping(&boot_cfg, bootstrap, kubeconfig, &store.current_path())
}

/// `writeKubeconfigFromBootstrapping`
fn write_kubeconfig_from_bootstrapping(
    boot: &k8s_client_go::Config,
    bootstrap_path: &Path,
    kubeconfig: &Path,
    pem_path: &Path,
) -> anyhow::Result<()> {
    // Keep the bootstrap file's CA reference (file or data).
    let raw: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(bootstrap_path)?)?;
    let cluster = raw["clusters"][0]["cluster"].clone();
    let mut c = serde_json::json!({"server": boot.server});
    if let Some(ca) = cluster.get("certificate-authority").and_then(|v| v.as_str()) {
        c["certificate-authority"] = serde_json::json!(ca);
    }
    if let Some(ca) = cluster.get("certificate-authority-data").and_then(|v| v.as_str()) {
        c["certificate-authority-data"] = serde_json::json!(ca);
    }
    if boot.tls.insecure {
        c["insecure-skip-tls-verify"] = serde_json::json!(true);
    }
    let pem = pem_path.display().to_string();
    let cfg = serde_json::json!({
        "apiVersion": "v1", "kind": "Config",
        "clusters": [{"name": "default-cluster", "cluster": c}],
        "users": [{"name": "default-auth", "user": {"client-certificate": pem, "client-key": pem}}],
        "contexts": [{"name": "default-context", "context": {"cluster": "default-cluster", "user": "default-auth", "namespace": "default"}}],
        "current-context": "default-context",
    });
    if let Some(dir) = kubeconfig.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_private(kubeconfig, serde_yaml::to_string(&cfg)?.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_and_template() {
        let dir = std::env::temp_dir().join(format!("kubelet-cert-test-{}", std::process::id()));
        let store = FileStore::new(&dir, "kubelet-client");
        assert!(store.current().is_none());
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["x".into()]).unwrap().self_signed(&key).unwrap().pem();
        store.update(cert.as_bytes(), key.serialize_pem().as_bytes()).unwrap();
        let (c, k) = store.current().unwrap();
        assert_eq!(validity(&c), validity(cert.as_bytes()));
        assert!(String::from_utf8(k).unwrap().contains("PRIVATE KEY"));
        assert!(std::fs::symlink_metadata(store.current_path()).unwrap().file_type().is_symlink());
        let t = Template::kubelet_client("n1");
        let csr = kube_apis_parse(&t.csr_pem(&key).unwrap());
        assert_eq!((csr.0.as_str(), csr.1), ("system:node:n1", vec!["system:nodes".to_string()]));
        let n = digested_name(&key.public_key_der(), &t);
        assert!(n.starts_with("node-csr-") && n == digested_name(&key.public_key_der(), &t));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn kube_apis_parse(pem: &str) -> (String, Vec<String>) {
        let b = Pem::iter_from_buffer(pem.as_bytes()).next().unwrap().unwrap();
        let der = b.contents.clone();
        let (_, r) =
            <x509_parser::certification_request::X509CertificationRequest as x509_parser::prelude::FromDer<_>>::from_der(&der).unwrap();
        let s = &r.certification_request_info.subject;
        let cn = s.iter_common_name().next().unwrap().as_str().unwrap().to_string();
        let orgs = s.iter_organization().map(|o| o.as_str().unwrap().to_string()).collect();
        (cn, orgs)
    }
}
