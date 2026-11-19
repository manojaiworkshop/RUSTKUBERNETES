//! Port of `pkg/controller/certificates/signer`: `certificatesigningrequest-signing-controller`.
//!
//! One signing loop per built-in signer, each with its CA (`--cluster-signing-cert-file`/
//! `--cluster-signing-key-file`, or the per-signer `--cluster-signing-<signer>-{cert,key}-file`).
//! Approved requests are checked against the signer's rules; a request that breaks
//! them gets a `Failed` condition (`SignerValidationFailure`), the others get
//! `status.certificate`. CA files are re-read when they change on disk.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use base64::Engine;
use k8s_api::certificates::v1::*;
use k8s_client_go::{Api, Client};
use kube_apis::certificates::{validate_kubelet_client_csr, validate_kubelet_serving_csr, ParsedCsr};

use super::authority::{CertificateAuthority, SigningPolicy};
use super::{condition, parse_request, CertificateController};
use crate::ControllerContext;

/// `CSRSigningControllerConfiguration`
#[derive(Clone, Debug, Default)]
pub struct CsrSigningConfig {
    pub cluster_signing_cert_file: String,
    pub cluster_signing_key_file: String,
    pub kubelet_serving: (String, String),
    pub kubelet_client: (String, String),
    pub kube_apiserver_client: (String, String),
    pub legacy_unknown: (String, String),
    /// `--cluster-signing-duration` (default 8760h).
    pub cluster_signing_duration: Duration,
}

impl CsrSigningConfig {
    fn any_specific(&self) -> bool {
        [&self.kubelet_serving, &self.kubelet_client, &self.kube_apiserver_client, &self.legacy_unknown]
            .iter()
            .any(|(c, k)| !c.is_empty() || !k.is_empty())
    }

    /// `get<Signer>SignerFiles`: specific files win; otherwise the cluster-wide ones,
    /// unless specific files are set for other signers.
    fn files_for(&self, specific: &(String, String)) -> (String, String) {
        if self.any_specific() {
            specific.clone()
        } else {
            (self.cluster_signing_cert_file.clone(), self.cluster_signing_key_file.clone())
        }
    }

    /// `Validate` of the options: default and per-signer files together are an error.
    pub fn validate(&self) -> Result<(), String> {
        let single = !self.cluster_signing_cert_file.is_empty() || !self.cluster_signing_key_file.is_empty();
        if single && self.any_specific() {
            return Err("cannot specify --cluster-signing-{cert,key}-file and other --cluster-signing-*-file flags at the same time".into());
        }
        for (name, (c, k)) in [
            ("cluster-signing-kubelet-serving", &self.kubelet_serving),
            ("cluster-signing-kubelet-client", &self.kubelet_client),
            ("cluster-signing-kube-apiserver-client", &self.kube_apiserver_client),
            ("cluster-signing-legacy-unknown", &self.legacy_unknown),
        ] {
            if c.is_empty() != k.is_empty() {
                return Err(format!("{name:?}: cert-file and key-file must be specified together"));
            }
        }
        Ok(())
    }
}

/// Modification times of the CA files and the CA loaded from them.
type Loaded = (Option<(SystemTime, SystemTime)>, Option<Arc<CertificateAuthority>>);

/// `caProvider`: the CA, reloaded when its files change.
struct CaProvider {
    cert_file: PathBuf,
    key_file: PathBuf,
    current: Mutex<Loaded>,
}

impl CaProvider {
    fn new(cert_file: &str, key_file: &str) -> Result<Self, String> {
        let p = Self { cert_file: cert_file.into(), key_file: key_file.into(), current: Mutex::new((None, None)) };
        p.current_ca()?;
        Ok(p)
    }

    fn mtimes(&self) -> Option<(SystemTime, SystemTime)> {
        let m = |p: &PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        Some((m(&self.cert_file)?, m(&self.key_file)?))
    }

    /// `currentCA`
    fn current_ca(&self) -> Result<Arc<CertificateAuthority>, String> {
        let stamp = self.mtimes();
        let mut cur = self.current.lock().unwrap();
        if let (Some(ca), true) = (&cur.1, cur.0 == stamp) {
            return Ok(ca.clone());
        }
        let cert = std::fs::read(&self.cert_file).map_err(|e| format!("error reading CA cert file {:?}: {e}", self.cert_file))?;
        let key = std::fs::read(&self.key_file).map_err(|e| format!("error reading CA key file {:?}: {e}", self.key_file))?;
        let ca = Arc::new(CertificateAuthority::load(&cert, &key)?);
        if cur.1.is_some() {
            tracing::info!(cert = %self.cert_file.display(), "reloaded CSR signing CA");
        }
        *cur = (stamp, Some(ca.clone()));
        Ok(ca)
    }
}

type IsRequestForSigner = fn(&ParsedCsr, &[String]) -> Result<(), String>;

/// `signer`
struct Signer {
    signer_name: &'static str,
    ca: CaProvider,
    cert_ttl: Duration,
    client: Client,
    validate: IsRequestForSigner,
}

/// `validAPIServerClientUsages`
fn valid_apiserver_client_usages(_req: &ParsedCsr, usages: &[String]) -> Result<(), String> {
    let mut client_auth = false;
    for u in usages {
        match u.as_str() {
            "digital signature" | "key encipherment" => {}
            "client auth" => client_auth = true,
            other => return Err(format!("invalid usage for client certificate: {other}")),
        }
    }
    if !client_auth {
        return Err("missing required usage for client certificate: client auth".into());
    }
    Ok(())
}

fn any_request(_req: &ParsedCsr, _usages: &[String]) -> Result<(), String> {
    Ok(())
}

impl Signer {
    /// `duration`: the requested `expirationSeconds`, between 10 minutes and the max TTL.
    fn duration(&self, expiration_seconds: Option<i32>) -> Duration {
        let Some(s) = expiration_seconds else { return self.cert_ttl };
        let requested = Duration::from_secs(s.max(0) as u64);
        if requested > self.cert_ttl {
            self.cert_ttl
        } else {
            requested.max(Duration::from_secs(600))
        }
    }

    /// `handle`
    async fn handle(&self, mut csr: CertificateSigningRequest) -> anyhow::Result<()> {
        if !csr.is_approved() || csr.has_true_condition(CERTIFICATE_FAILED) {
            return Ok(());
        }
        if csr.spec.signer_name != self.signer_name {
            return Ok(());
        }
        let req = parse_request(&csr)?;
        let usages = csr.spec.usages.clone().unwrap_or_default();
        let api: Api<CertificateSigningRequest> = Api::all(self.client.clone());
        if let Err(e) = (self.validate)(&req, &usages) {
            let status = csr.status.get_or_insert_with(Default::default);
            status.conditions.get_or_insert_with(Vec::new).push(condition(CERTIFICATE_FAILED, "SignerValidationFailure", &e));
            api.replace_status(&csr).await.map_err(|e| anyhow::anyhow!("error adding failure condition for csr: {e}"))?;
            return Ok(());
        }
        let ca = self.ca.current_ca().map_err(anyhow::Error::msg)?;
        let policy = SigningPolicy {
            ttl: self.duration(csr.spec.expiration_seconds),
            usages,
            // Must be less than the minimum TTL a user may request (10 minutes).
            backdate: Duration::from_secs(5 * 60),
            // 5 minutes of backdating is roughly 1% of 8 hours.
            short: Duration::from_secs(8 * 3600),
            now: chrono::Utc::now(),
        };
        let der = ca.sign(&req, &policy).map_err(|e| anyhow::anyhow!("error auto signing csr: {e}"))?;
        let pem = pem_encode("CERTIFICATE", &der);
        csr.status.get_or_insert_with(Default::default).certificate = Some(base64::engine::general_purpose::STANDARD.encode(pem));
        api.replace_status(&csr).await.map_err(|e| anyhow::anyhow!("error updating signature for csr: {e}"))?;
        tracing::info!(csr = csr.metadata.name.as_deref().unwrap_or_default(), signer = self.signer_name, "signed certificate");
        Ok(())
    }
}

/// `pem.EncodeToMemory`
pub fn pem_encode(label: &str, der: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// `newCertificateSigningRequestSigningController`: the signing loops to run, or
/// none ("Skipping CSR signer controller because no csr cert/key was specified").
pub fn new_controllers(ctx: &ControllerContext) -> Result<Vec<Arc<CertificateController>>, String> {
    let cfg = &ctx.config.csr_signing;
    let missing_single = cfg.cluster_signing_cert_file.is_empty() || cfg.cluster_signing_key_file.is_empty();
    if missing_single && !cfg.any_specific() {
        tracing::info!("Skipping CSR signer controller because no csr cert/key was specified");
        return Ok(vec![]);
    }
    if !missing_single && cfg.any_specific() {
        return Err("cannot specify default and per controller certs at the same time".into());
    }
    let signers: [(&'static str, &'static str, &(String, String), IsRequestForSigner); 4] = [
        ("csrsigning-kubelet-serving", KUBELET_SERVING_SIGNER_NAME, &cfg.kubelet_serving, |r, u| validate_kubelet_serving_csr(r, u)),
        ("csrsigning-kubelet-client", KUBE_APISERVER_CLIENT_KUBELET_SIGNER_NAME, &cfg.kubelet_client, |r, u| {
            validate_kubelet_client_csr(r, u)
        }),
        ("csrsigning-kube-apiserver-client", KUBE_APISERVER_CLIENT_SIGNER_NAME, &cfg.kube_apiserver_client, valid_apiserver_client_usages),
        ("csrsigning-legacy-unknown", LEGACY_UNKNOWN_SIGNER_NAME, &cfg.legacy_unknown, any_request),
    ];
    let mut out = vec![];
    for (name, signer_name, specific, validate) in signers {
        let (cert, key) = cfg.files_for(specific);
        if cert.is_empty() && key.is_empty() {
            tracing::info!(
                controller = signer_name,
                "Skipping CSR signer controller because specific files were specified for other signers and not this one"
            );
            continue;
        }
        let ca = CaProvider::new(&cert, &key).map_err(|e| format!("failed to init {signer_name} certificate controller: {e}"))?;
        let s = Arc::new(Signer { signer_name, ca, cert_ttl: cfg.cluster_signing_duration, client: ctx.client.clone(), validate });
        out.push(CertificateController::new(
            name,
            ctx,
            Arc::new(move |csr| {
                let s = s.clone();
                Box::pin(async move { s.handle(csr).await })
            }),
        ));
    }
    Ok(out)
}
