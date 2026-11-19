//! TLS helpers (`client-go/util/cert`, `transport`): PEM loading and rustls
//! client/server configurations. All components use the `ring` crypto provider.

use std::sync::{Arc, Once};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, ServerConfig, SignatureScheme};

/// Installs `ring` as the process-wide rustls provider (idempotent).
pub fn install_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    install_crypto_provider();
    Arc::new(rustls::crypto::ring::default_provider())
}

pub fn parse_certs(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, String> {
    let certs: Vec<_> = CertificateDer::pem_slice_iter(pem).collect::<Result<_, _>>().map_err(|e| format!("parsing certificates: {e}"))?;
    if certs.is_empty() {
        return Err("no certificates found in PEM data".into());
    }
    Ok(certs)
}

pub fn parse_key(pem: &[u8]) -> Result<PrivateKeyDer<'static>, String> {
    PrivateKeyDer::from_pem_slice(pem).map_err(|e| format!("parsing private key: {e}"))
}

pub fn read_file(path: &str) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("reading {path}: {e}"))
}

fn root_store(ca_pem: &[u8]) -> Result<RootCertStore, String> {
    let mut roots = RootCertStore::empty();
    for c in parse_certs(ca_pem)? {
        roots.add(c).map_err(|e| format!("adding CA certificate: {e}"))?;
    }
    Ok(roots)
}

/// TLS material for a client connection (`rest.TLSClientConfig`).
#[derive(Clone, Default)]
pub struct TlsClientConfig {
    /// PEM CA bundle used to verify the server; `None` uses no roots (must set `insecure`).
    pub ca_data: Option<Vec<u8>>,
    /// PEM client certificate + key (x509 client authentication).
    pub cert_data: Option<Vec<u8>>,
    pub key_data: Option<Vec<u8>>,
    /// Skip server certificate verification (`insecure-skip-tls-verify`).
    pub insecure: bool,
    /// A client certificate that can change at runtime (kubelet certificate
    /// rotation, `transport.Config.TLS.GetCert`); wins over `cert_data`.
    pub cert_resolver: Option<Arc<DynamicCert>>,
}

impl std::fmt::Debug for TlsClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsClientConfig")
            .field("ca_data", &self.ca_data.as_ref().map(|_| "<pem>"))
            .field("cert_data", &self.cert_data.as_ref().map(|_| "<pem>"))
            .field("key_data", &self.key_data.as_ref().map(|_| "<redacted>"))
            .field("insecure", &self.insecure)
            .field("cert_resolver", &self.cert_resolver.is_some())
            .finish()
    }
}

impl TlsClientConfig {
    pub fn rustls(&self) -> Result<ClientConfig, String> {
        let builder = ClientConfig::builder_with_provider(provider()).with_safe_default_protocol_versions().map_err(|e| e.to_string())?;
        let builder = if self.insecure {
            builder.dangerous().with_custom_certificate_verifier(Arc::new(NoVerify(provider())))
        } else {
            let ca = self
                .ca_data
                .as_deref()
                .ok_or("no certificate authority configured (certificate-authority-data) and insecure-skip-tls-verify is not set")?;
            builder.with_root_certificates(root_store(ca)?)
        };
        let mut cfg = match (&self.cert_data, &self.key_data) {
            _ if self.cert_resolver.is_some() => builder.with_client_cert_resolver(self.cert_resolver.clone().unwrap()),
            (Some(c), Some(k)) => {
                builder.with_client_auth_cert(parse_certs(c)?, parse_key(k)?).map_err(|e| format!("client certificate: {e}"))?
            }
            _ => builder.with_no_client_auth(),
        };
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(cfg)
    }
}

/// Server TLS config; with `client_ca`, client certificates are requested and
/// verified but optional (other authenticators may still apply).
pub fn server_config(cert_pem: &[u8], key_pem: &[u8], client_ca: Option<&[u8]>) -> Result<ServerConfig, String> {
    let builder = ServerConfig::builder_with_provider(provider()).with_safe_default_protocol_versions().map_err(|e| e.to_string())?;
    let builder = match client_ca {
        Some(ca) => {
            let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(root_store(ca)?), provider())
                .allow_unauthenticated()
                .build()
                .map_err(|e| format!("client CA: {e}"))?;
            builder.with_client_cert_verifier(verifier)
        }
        None => builder.with_no_client_auth(),
    };
    let mut cfg = builder.with_single_cert(parse_certs(cert_pem)?, parse_key(key_pem)?).map_err(|e| format!("serving certificate: {e}"))?;
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(cfg)
}

/// Server TLS config whose certificate comes from `cert` at every handshake
/// (rotated kubelet serving certificates).
pub fn server_config_dynamic(cert: Arc<DynamicCert>, client_ca: Option<&[u8]>) -> Result<ServerConfig, String> {
    let builder = ServerConfig::builder_with_provider(provider()).with_safe_default_protocol_versions().map_err(|e| e.to_string())?;
    let builder = match client_ca {
        Some(ca) => {
            let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(root_store(ca)?), provider())
                .allow_unauthenticated()
                .build()
                .map_err(|e| format!("client CA: {e}"))?;
            builder.with_client_cert_verifier(verifier)
        }
        None => builder.with_no_client_auth(),
    };
    let mut cfg = builder.with_cert_resolver(cert);
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(cfg)
}

/// A certificate and key that can be replaced while connections use it.
#[derive(Default)]
pub struct DynamicCert {
    current: std::sync::RwLock<Option<Arc<rustls::sign::CertifiedKey>>>,
}

impl std::fmt::Debug for DynamicCert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynamicCert").field("loaded", &self.current().is_some()).finish()
    }
}

impl DynamicCert {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Replaces the certificate (PEM chain) and key (PEM).
    pub fn set(&self, cert_pem: &[u8], key_pem: &[u8]) -> Result<(), String> {
        let certs = parse_certs(cert_pem)?;
        let key = provider().key_provider.load_private_key(parse_key(key_pem)?).map_err(|e| format!("loading private key: {e}"))?;
        *self.current.write().unwrap() = Some(Arc::new(rustls::sign::CertifiedKey::new(certs, key)));
        Ok(())
    }

    pub fn current(&self) -> Option<Arc<rustls::sign::CertifiedKey>> {
        self.current.read().unwrap().clone()
    }
}

impl rustls::client::ResolvesClientCert for DynamicCert {
    fn resolve(&self, _hints: &[&[u8]], _schemes: &[SignatureScheme]) -> Option<Arc<rustls::sign::CertifiedKey>> {
        self.current()
    }

    fn has_certs(&self) -> bool {
        self.current().is_some()
    }
}

impl rustls::server::ResolvesServerCert for DynamicCert {
    fn resolve(&self, _hello: rustls::server::ClientHello<'_>) -> Option<Arc<rustls::sign::CertifiedKey>> {
        self.current()
    }
}

/// Accepts any server certificate (signatures are still checked).
#[derive(Debug)]
struct NoVerify(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
