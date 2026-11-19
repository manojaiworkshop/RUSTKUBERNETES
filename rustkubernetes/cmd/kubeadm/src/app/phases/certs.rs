//! `kubeadm init phase certs` (`cmd/kubeadm/app/phases/certs`): the cluster CA
//! and the certificates every component needs, written in kubeadm's layout:
//!
//! | File | Subject | Use |
//! |------|---------|-----|
//! | `ca.crt/key` | CN=kubernetes | signs everything below |
//! | `apiserver.crt/key` | CN=kube-apiserver, SANs | API server serving cert |
//! | `apiserver-kubelet-client.crt/key` | CN=kube-apiserver-kubelet-client, O=kubeadm:cluster-admins | API server → kubelet |
//! | `kubelet.crt/key` | CN=<node>, SANs | kubelet serving cert (signed, so the API server can verify it) |
//! | `sa.key/pub` | — | ServiceAccount token signing (ES256) |
//!
//! Existing files are kept, so the phase can be re-run (e.g. to add a node).

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
    SanType,
};

/// `CACertificateValidityPeriod`: 10 years.
pub const CA_VALIDITY: time::Duration = time::Duration::days(365 * 10);
/// `CertificateValidityPeriod`: 1 year.
pub const CERT_VALIDITY: time::Duration = time::Duration::days(365);
/// `CertificateBackdate`
pub const CERT_BACKDATE: time::Duration = time::Duration::minutes(5);

/// The CA, loaded or created.
pub struct Ca {
    pub cert: Certificate,
    pub key: KeyPair,
    pub pem: String,
}

/// `NewSelfSignedCACert`: now → now + 10 years.
fn ca_validity(p: &mut CertificateParams) {
    let now = time::OffsetDateTime::now_utc();
    p.not_before = now;
    p.not_after = now + CA_VALIDITY;
}

/// Signed certificates: now − 5 minutes → now + 1 year (kubeadm's renewal window,
/// also used for new certificates so they never start before the CA).
fn leaf_validity(p: &mut CertificateParams) {
    let now = time::OffsetDateTime::now_utc();
    p.not_before = now - CERT_BACKDATE;
    p.not_after = now + CERT_VALIDITY;
}

fn dn(cn: &str, orgs: &[&str]) -> DistinguishedName {
    let mut d = DistinguishedName::new();
    for o in orgs {
        d.push(DnType::OrganizationName, *o);
    }
    d.push(DnType::CommonName, cn);
    d
}

fn write(path: &Path, data: &str, private: bool) -> Result<()> {
    std::fs::write(path, data).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Loads `ca.crt`/`ca.key` from `dir`, creating them if missing.
pub fn ensure_ca(dir: &Path) -> Result<Ca> {
    ensure_named_ca(dir, "kubernetes", "ca")
}

/// A CA in `dir` with common name `cn` (`kubernetes`, or `etcd-ca` for `pki/etcd`).
pub fn ensure_named_ca(dir: &Path, cn: &str, label: &str) -> Result<Ca> {
    ensure_ca_files(dir, "ca", cn, label)
}

/// A CA stored as `<dir>/<base>.crt` and `<base>.key` (e.g. `front-proxy-ca`).
pub fn ensure_ca_files(dir: &Path, base: &str, cn: &str, label: &str) -> Result<Ca> {
    std::fs::create_dir_all(dir)?;
    let (crt, key) = (dir.join(format!("{base}.crt")), dir.join(format!("{base}.key")));
    if crt.exists() && key.exists() {
        let pem = std::fs::read_to_string(&crt)?;
        let key = KeyPair::from_pem(&std::fs::read_to_string(&key)?).with_context(|| format!("parsing {base}.key"))?;
        // Re-create an issuer with the same subject and key (signatures stay valid).
        let cert = CertificateParams::from_ca_cert_pem(&pem).with_context(|| format!("parsing {base}.crt"))?.self_signed(&key)?;
        return Ok(Ca { cert, key, pem });
    }
    let key = KeyPair::generate()?;
    let mut p = CertificateParams::default();
    p.distinguished_name = dn(cn, &[]);
    p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    ca_validity(&mut p);
    let cert = p.self_signed(&key)?;
    let pem = cert.pem();
    write(&crt, &pem, false)?;
    write(&key_path(&crt), &key.serialize_pem(), true)?;
    println!("[certs] Generating \"{label}\" certificate and key");
    Ok(Ca { cert, key, pem })
}

fn key_path(crt: &Path) -> PathBuf {
    crt.with_extension("key")
}

/// What a leaf certificate is for.
pub struct LeafSpec<'a> {
    pub cn: &'a str,
    pub orgs: &'a [&'a str],
    pub dns: Vec<String>,
    pub ips: Vec<IpAddr>,
    pub server: bool,
    pub client: bool,
}

/// Issues a certificate signed by the CA; returns (cert PEM, key PEM).
pub fn issue(ca: &Ca, spec: &LeafSpec) -> Result<(String, String)> {
    let key = KeyPair::generate()?;
    let mut p = CertificateParams::default();
    p.distinguished_name = dn(spec.cn, spec.orgs);
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
    if spec.server {
        p.extended_key_usages.push(ExtendedKeyUsagePurpose::ServerAuth);
    }
    if spec.client {
        p.extended_key_usages.push(ExtendedKeyUsagePurpose::ClientAuth);
    }
    for d in &spec.dns {
        p.subject_alt_names.push(SanType::DnsName(d.as_str().try_into()?));
    }
    for ip in &spec.ips {
        p.subject_alt_names.push(SanType::IpAddress(*ip));
    }
    leaf_validity(&mut p);
    let cert = p.signed_by(&key, &ca.cert, &ca.key)?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// Writes `<name>.crt`/`<name>.key` unless both exist.
fn ensure_leaf(dir: &Path, name: &str, ca: &Ca, spec: &LeafSpec) -> Result<()> {
    let crt = dir.join(format!("{name}.crt"));
    if crt.exists() && key_path(&crt).exists() {
        println!("[certs] Using existing \"{name}\" certificate and key");
        return Ok(());
    }
    let (c, k) = issue(ca, spec)?;
    write(&crt, &c, false)?;
    write(&key_path(&crt), &k, true)?;
    let sans: Vec<String> = spec.dns.iter().cloned().chain(spec.ips.iter().map(ToString::to_string)).collect();
    if sans.is_empty() {
        println!("[certs] Generating \"{name}\" certificate and key");
    } else {
        println!("[certs] Generating \"{name}\" certificate and key; serving cert is signed for {}", sans.join(", "));
    }
    Ok(())
}

pub struct CertOptions {
    pub cert_dir: PathBuf,
    pub advertise_address: IpAddr,
    pub service_cluster_ip: IpAddr,
    pub dns_domain: String,
    pub extra_sans: Vec<String>,
    pub node_name: String,
    pub node_ips: Vec<IpAddr>,
}

fn split_sans(extra: &[String]) -> (Vec<String>, Vec<IpAddr>) {
    let (mut dns, mut ips) = (vec![], vec![]);
    for s in extra.iter().filter(|s| !s.is_empty()) {
        match s.parse::<IpAddr>() {
            Ok(ip) => ips.push(ip),
            Err(_) => dns.push(s.clone()),
        }
    }
    (dns, ips)
}

/// `kubeadm init phase certs all`.
pub fn all(o: &CertOptions) -> Result<Ca> {
    let ca = ensure_ca(&o.cert_dir)?;
    let (extra_dns, extra_ips) = split_sans(&o.extra_sans);
    let mut dns: Vec<String> = ["kubernetes", "kubernetes.default", "kubernetes.default.svc"].iter().map(|s| s.to_string()).collect();
    dns.push(format!("kubernetes.default.svc.{}", o.dns_domain));
    dns.push("localhost".into());
    dns.push(o.node_name.clone());
    dns.extend(extra_dns.clone());
    let mut ips = vec![o.service_cluster_ip, o.advertise_address, "127.0.0.1".parse()?];
    ips.extend(extra_ips.clone());
    ips.dedup();
    ensure_leaf(&o.cert_dir, "apiserver", &ca, &LeafSpec { cn: "kube-apiserver", orgs: &[], dns, ips, server: true, client: false })?;
    ensure_leaf(
        &o.cert_dir,
        "apiserver-kubelet-client",
        &ca,
        &LeafSpec {
            cn: "kube-apiserver-kubelet-client",
            orgs: &["kubeadm:cluster-admins"],
            dns: vec![],
            ips: vec![],
            server: false,
            client: true,
        },
    )?;
    kubelet_serving(o, &ca)?;
    ensure_sa_keys(&o.cert_dir)?;
    front_proxy(o)?;
    etcd(o)?;
    Ok(ca)
}

/// kubeadm's front-proxy PKI for the aggregation layer: `front-proxy-ca`
/// (CN `front-proxy-ca`) and `front-proxy-client` (CN `front-proxy-client`, client auth),
/// which the API server presents to extension API servers such as metrics-server.
pub fn front_proxy(o: &CertOptions) -> Result<()> {
    let ca = ensure_ca_files(&o.cert_dir, "front-proxy-ca", "front-proxy-ca", "front-proxy-ca")?;
    ensure_leaf(
        &o.cert_dir,
        "front-proxy-client",
        &ca,
        &LeafSpec { cn: "front-proxy-client", orgs: &[], dns: vec![], ips: vec![], server: false, client: true },
    )
}

/// kubeadm's etcd PKI: `etcd/ca` (CN `etcd-ca`), `etcd/server` and `etcd/peer`
/// (CN = node name; SANs node name, localhost, node IPs, 127.0.0.1, ::1),
/// `etcd/healthcheck-client` and `apiserver-etcd-client` (both client certs from the etcd CA).
pub fn etcd(o: &CertOptions) -> Result<()> {
    let dir = o.cert_dir.join("etcd");
    let ca = ensure_named_ca(&dir, "etcd-ca", "etcd/ca")?;
    let mut ips = vec![o.advertise_address, "127.0.0.1".parse()?, "::1".parse()?];
    ips.extend(o.node_ips.iter().copied());
    ips.dedup();
    let dns = vec![o.node_name.clone(), "localhost".into()];
    let both = LeafSpec { cn: &o.node_name, orgs: &[], dns, ips, server: true, client: true };
    ensure_leaf(&dir, "server", &ca, &both)?;
    ensure_leaf(&dir, "peer", &ca, &both)?;
    ensure_leaf(
        &dir,
        "healthcheck-client",
        &ca,
        &LeafSpec { cn: "kube-etcd-healthcheck-client", orgs: &[], dns: vec![], ips: vec![], server: false, client: true },
    )?;
    ensure_leaf(
        &o.cert_dir,
        "apiserver-etcd-client",
        &ca,
        &LeafSpec { cn: "kube-apiserver-etcd-client", orgs: &[], dns: vec![], ips: vec![], server: false, client: true },
    )?;
    Ok(())
}

/// Serving certificate for the kubelet on `node_name` (`kubelet-<node>.crt` or `kubelet.crt`).
pub fn kubelet_serving(o: &CertOptions, ca: &Ca) -> Result<()> {
    let mut ips = o.node_ips.clone();
    ips.push("127.0.0.1".parse()?);
    ips.dedup();
    ensure_leaf(
        &o.cert_dir,
        "kubelet",
        ca,
        &LeafSpec { cn: &o.node_name, orgs: &[], dns: vec![o.node_name.clone(), "localhost".into()], ips, server: true, client: false },
    )
}

/// `sa.key` (PKCS#8 ECDSA P-256) and `sa.pub` (SPKI), used for ServiceAccount tokens.
pub fn ensure_sa_keys(dir: &Path) -> Result<()> {
    let (k, p) = (dir.join("sa.key"), dir.join("sa.pub"));
    if k.exists() && p.exists() {
        println!("[certs] Using the existing \"sa\" key");
        return Ok(());
    }
    let key = KeyPair::generate()?;
    write(&k, &key.serialize_pem(), true)?;
    write(&p, &key.public_key_pem(), false)?;
    println!("[certs] Generating \"sa\" key and public key");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use x509_parser::prelude::*;

    #[test]
    fn issues_and_reloads() {
        let dir = std::env::temp_dir().join(format!("kubeadm-certs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let o = CertOptions {
            cert_dir: dir.clone(),
            advertise_address: "172.17.0.1".parse().unwrap(),
            service_cluster_ip: "10.96.0.1".parse().unwrap(),
            dns_domain: "cluster.local".into(),
            extra_sans: vec!["example.test".into(), "192.0.2.5".into()],
            node_name: "node-a".into(),
            node_ips: vec!["127.0.0.1".parse().unwrap()],
        };
        all(&o).unwrap();
        let pem = std::fs::read(dir.join("apiserver.crt")).unwrap();
        let (_, p) = parse_x509_pem(&pem).unwrap();
        let cert = p.parse_x509().unwrap();
        let san = cert
            .subject_alternative_name()
            .unwrap()
            .unwrap()
            .value
            .general_names
            .iter()
            .map(|g| format!("{g}"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(
            san.contains("kubernetes.default.svc.cluster.local") && san.contains("0a:60:00:01") && san.contains("example.test"),
            "{san}"
        );
        // Re-running with an existing CA issues certificates that chain to the same CA.
        let ca = ensure_ca(&dir).unwrap();
        let (c, _) = issue(&ca, &LeafSpec { cn: "jane", orgs: &["dev"], dns: vec![], ips: vec![], server: false, client: true }).unwrap();
        let (_, p) = parse_x509_pem(c.as_bytes()).unwrap();
        let leaf = p.parse_x509().unwrap();
        let (_, cap) = parse_x509_pem(&std::fs::read(dir.join("ca.crt")).unwrap()).unwrap();
        let ca_cert = cap.parse_x509().unwrap();
        leaf.verify_signature(Some(ca_cert.public_key())).unwrap();
        assert_eq!(leaf.subject().iter_common_name().next().unwrap().as_str().unwrap(), "jane");
        std::fs::remove_dir_all(dir).ok();
    }
}
