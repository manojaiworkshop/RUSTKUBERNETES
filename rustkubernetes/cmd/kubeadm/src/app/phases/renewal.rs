//! Port of `cmd/kubeadm/app/phases/certs/renewal` and `kubeadm certs
//! check-expiration` / `kubeadm certs renew`.
//!
//! Every certificate is renewed with the local CA: a new key, the same subject,
//! SANs and usages, valid from now − 5 minutes for one year. Kubeconfig files
//! get their embedded client certificate and key replaced.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::Engine;
use rcgen::{CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, KeyPair, KeyUsagePurpose, SanType};
use x509_parser::pem::Pem;

use super::certs::{ensure_ca_files, Ca, CERT_BACKDATE, CERT_VALIDITY};

/// Where a certificate lives.
#[derive(Clone, Debug)]
pub enum Location {
    /// `<cert-dir>/<base>.crt`
    Pki(&'static str),
    /// Embedded in `<kubeconfig-dir>/<file>`.
    Kubeconfig(&'static str),
}

/// `CertificateRenewHandler`
#[derive(Clone, Debug)]
pub struct Handler {
    pub name: &'static str,
    pub long_name: &'static str,
    pub location: Location,
    /// CA name (`ca`, `etcd-ca`, `front-proxy-ca`).
    pub ca_name: &'static str,
}

/// CAs: (name, base path in the cert dir).
pub const CAS: &[(&str, &str)] = &[("ca", "ca"), ("etcd-ca", "etcd/ca"), ("front-proxy-ca", "front-proxy-ca")];

/// `Manager.Certificates()`, sorted by name. `kube-proxy.conf`, `kubelet` and
/// `kubelet.conf` are this project's (upstream's kubelet rotates its own and
/// kube-proxy uses a ServiceAccount); they are listed when present.
pub fn handlers() -> Vec<Handler> {
    let h = |name, long_name, location, ca_name| Handler { name, long_name, location, ca_name };
    let mut v = vec![
        h(
            "admin.conf",
            "certificate embedded in the kubeconfig file for the admin to use and for kubeadm itself",
            Location::Kubeconfig("admin.conf"),
            "ca",
        ),
        h("apiserver", "certificate for serving the Kubernetes API", Location::Pki("apiserver"), "ca"),
        h("apiserver-etcd-client", "certificate the apiserver uses to access etcd", Location::Pki("apiserver-etcd-client"), "etcd-ca"),
        h(
            "apiserver-kubelet-client",
            "certificate for the API server to connect to kubelet",
            Location::Pki("apiserver-kubelet-client"),
            "ca",
        ),
        h(
            "controller-manager.conf",
            "certificate embedded in the kubeconfig file for the controller manager to use",
            Location::Kubeconfig("controller-manager.conf"),
            "ca",
        ),
        h(
            "etcd-healthcheck-client",
            "certificate for liveness probes to healthcheck etcd",
            Location::Pki("etcd/healthcheck-client"),
            "etcd-ca",
        ),
        h("etcd-peer", "certificate for etcd nodes to communicate with each other", Location::Pki("etcd/peer"), "etcd-ca"),
        h("etcd-server", "certificate for serving etcd", Location::Pki("etcd/server"), "etcd-ca"),
        h("front-proxy-client", "certificate for the front proxy client", Location::Pki("front-proxy-client"), "front-proxy-ca"),
        h(
            "kube-proxy.conf",
            "certificate embedded in the kubeconfig file for kube-proxy to use",
            Location::Kubeconfig("kube-proxy.conf"),
            "ca",
        ),
        h("kubelet", "certificate for serving the kubelet API", Location::Pki("kubelet"), "ca"),
        h("kubelet.conf", "certificate embedded in the kubeconfig file for the kubelet to use", Location::Kubeconfig("kubelet.conf"), "ca"),
        h(
            "scheduler.conf",
            "certificate embedded in the kubeconfig file for the scheduler manager to use",
            Location::Kubeconfig("scheduler.conf"),
            "ca",
        ),
        h(
            "super-admin.conf",
            "certificate embedded in the kubeconfig file for the super-admin",
            Location::Kubeconfig("super-admin.conf"),
            "ca",
        ),
    ];
    v.sort_by_key(|h| h.name);
    v
}

/// Directories the manager works on.
pub struct Dirs {
    pub cert_dir: PathBuf,
    pub kubeconfig_dir: PathBuf,
}

fn first_cert(pem: &[u8]) -> Result<Vec<u8>> {
    let b = Pem::iter_from_buffer(pem).next().context("no PEM data")?.context("invalid PEM")?;
    Ok(b.contents)
}

fn kubeconfig_cert(path: &Path) -> Result<Option<(serde_yaml::Value, Vec<u8>)>> {
    if !path.exists() {
        return Ok(None);
    }
    let y: serde_yaml::Value = serde_yaml::from_str(&std::fs::read_to_string(path)?)?;
    let Some(data) = y["users"][0]["user"]["client-certificate-data"].as_str() else { return Ok(None) };
    let pem = base64::engine::general_purpose::STANDARD.decode(data)?;
    Ok(Some((y, pem)))
}

impl Dirs {
    fn ca(&self, name: &str) -> Result<Ca> {
        let base = CAS.iter().find(|(n, _)| *n == name).map(|(_, b)| *b).unwrap();
        let (dir, file) = match base.rsplit_once('/') {
            Some((d, f)) => (self.cert_dir.join(d), f),
            None => (self.cert_dir.clone(), base),
        };
        if !dir.join(format!("{file}.key")).exists() {
            bail!("couldn't load the certificate authority {name}: {}.key not found (external CA)", dir.join(file).display());
        }
        ensure_ca_files(&dir, file, name, name)
    }

    /// The certificate PEM, if present.
    pub fn read(&self, h: &Handler) -> Result<Option<Vec<u8>>> {
        match h.location {
            Location::Pki(base) => {
                let p = self.cert_dir.join(format!("{base}.crt"));
                Ok(p.exists().then(|| std::fs::read(&p)).transpose()?)
            }
            Location::Kubeconfig(f) => Ok(kubeconfig_cert(&self.kubeconfig_dir.join(f))?.map(|(_, c)| c)),
        }
    }

    /// `RenewUsingLocalCA`
    pub fn renew(&self, h: &Handler) -> Result<()> {
        let pem = self.read(h)?.context("missing")?;
        let der = first_cert(&pem)?;
        let (_, old) = x509_parser::parse_x509_certificate(&der).map_err(|e| anyhow::anyhow!("parsing {}: {e}", h.name))?;
        let ca = self.ca(h.ca_name)?;
        let (cert, key) = reissue(&old, &ca)?;
        match h.location {
            Location::Pki(base) => {
                let crt = self.cert_dir.join(format!("{base}.crt"));
                std::fs::write(&crt, &cert)?;
                write_private(&crt.with_extension("key"), key.as_bytes())?;
            }
            Location::Kubeconfig(f) => {
                let path = self.kubeconfig_dir.join(f);
                let (mut y, _) = kubeconfig_cert(&path)?.context("missing")?;
                let b64 = |s: &str| serde_yaml::Value::String(base64::engine::general_purpose::STANDARD.encode(s));
                y["users"][0]["user"]["client-certificate-data"] = b64(&cert);
                y["users"][0]["user"]["client-key-data"] = b64(&key);
                write_private(&path, serde_yaml::to_string(&y)?.as_bytes())?;
            }
        }
        Ok(())
    }
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    std::fs::write(path, data).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// A new key and certificate with `old`'s subject, SANs and usages.
fn reissue(old: &x509_parser::certificate::X509Certificate<'_>, ca: &Ca) -> Result<(String, String)> {
    let key = KeyPair::generate()?;
    let mut p = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    for o in old.subject().iter_organization() {
        dn.push(DnType::OrganizationName, o.as_str().unwrap_or_default());
    }
    if let Some(cn) = old.subject().iter_common_name().next() {
        dn.push(DnType::CommonName, cn.as_str().unwrap_or_default());
    }
    p.distinguished_name = dn;
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
    if let Ok(Some(eku)) = old.extended_key_usage() {
        if eku.value.server_auth {
            p.extended_key_usages.push(ExtendedKeyUsagePurpose::ServerAuth);
        }
        if eku.value.client_auth {
            p.extended_key_usages.push(ExtendedKeyUsagePurpose::ClientAuth);
        }
    }
    if let Ok(Some(san)) = old.subject_alternative_name() {
        for n in &san.value.general_names {
            match n {
                x509_parser::extensions::GeneralName::DNSName(d) => p.subject_alt_names.push(SanType::DnsName((*d).try_into()?)),
                x509_parser::extensions::GeneralName::IPAddress(b) => {
                    let ip = match b.len() {
                        4 => std::net::IpAddr::from(<[u8; 4]>::try_from(*b)?),
                        16 => std::net::IpAddr::from(<[u8; 16]>::try_from(*b)?),
                        _ => continue,
                    };
                    p.subject_alt_names.push(SanType::IpAddress(ip));
                }
                _ => {}
            }
        }
    }
    let now = time::OffsetDateTime::now_utc();
    p.not_before = now - CERT_BACKDATE;
    p.not_after = now + CERT_VALIDITY;
    let cert = p.signed_by(&key, &ca.cert, &ca.key)?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// (expiration, residual seconds)
fn expiry(pem: &[u8]) -> Result<(chrono::DateTime<chrono::Utc>, i64)> {
    let der = first_cert(pem)?;
    let (_, c) = x509_parser::parse_x509_certificate(&der).map_err(|e| anyhow::anyhow!("{e}"))?;
    let na = c.validity().not_after.timestamp();
    Ok((chrono::DateTime::from_timestamp(na, 0).unwrap_or_default(), na - chrono::Utc::now().timestamp()))
}

/// `kubeadm certs check-expiration` (text printer).
pub fn check_expiration(d: &Dirs) -> Result<String> {
    let fmt = |t: chrono::DateTime<chrono::Utc>| t.format("%b %d, %Y %H:%M UTC").to_string();
    let human = |s: i64| k8s_apimachinery::meta::human_duration(s);
    let mut rows = vec![vec![
        "CERTIFICATE".to_string(),
        "EXPIRES".into(),
        "RESIDUAL TIME".into(),
        "CERTIFICATE AUTHORITY".into(),
        "EXTERNALLY MANAGED".into(),
    ]];
    for h in handlers() {
        match d.read(&h)? {
            Some(pem) => {
                let (exp, left) = expiry(&pem)?;
                let external = d.ca(h.ca_name).is_err();
                rows.push(vec![h.name.into(), fmt(exp), human(left), h.ca_name.into(), if external { "yes" } else { "no" }.into()]);
            }
            // Optional extras of this project are left out when absent.
            None if matches!(h.name, "kube-proxy.conf" | "kubelet" | "kubelet.conf") => {}
            None => rows.push(vec![format!("!MISSING! {}", h.name), String::new(), String::new(), String::new(), String::new()]),
        }
    }
    let mut out = tabwrite(&rows);
    out.push('\n');
    let mut cas = vec![vec!["CERTIFICATE AUTHORITY".to_string(), "EXPIRES".into(), "RESIDUAL TIME".into(), "EXTERNALLY MANAGED".into()]];
    for (name, base) in CAS {
        let p = d.cert_dir.join(format!("{base}.crt"));
        if !p.exists() {
            cas.push(vec![format!("!MISSING! {name}"), String::new(), String::new(), String::new()]);
            continue;
        }
        let (exp, left) = expiry(&std::fs::read(&p)?)?;
        let external = !d.cert_dir.join(format!("{base}.key")).exists();
        cas.push(vec![name.to_string(), fmt(exp), human(left), if external { "yes" } else { "no" }.into()]);
    }
    out.push_str(&tabwrite(&cas));
    Ok(out)
}

/// `tabwriter.NewWriter(w, 10, 4, 3, ' ', 0)`: columns padded to the widest
/// cell + 3 spaces (at least 10 wide); the last column is not padded.
fn tabwrite(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols).map(|c| rows.iter().map(|r| r.get(c).map_or(0, |s| s.chars().count())).max().unwrap_or(0)).collect();
    let mut out = String::new();
    for r in rows {
        let mut line = String::new();
        for (i, cell) in r.iter().enumerate() {
            if i + 1 == r.len() {
                line.push_str(cell);
            } else {
                let w = (widths[i] + 3).max(10);
                line.push_str(&format!("{cell:<w$}"));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// `kubeadm certs renew <name>` / `all`.
pub fn renew(d: &Dirs, which: &str) -> Result<()> {
    let all = handlers();
    let selected: Vec<Handler> = if which == "all" { all } else { all.into_iter().filter(|h| h.name == which).collect() };
    if selected.is_empty() {
        bail!("unknown certificate {which:?}");
    }
    for h in &selected {
        if d.read(h)?.is_none() {
            if !(which == "all" && matches!(h.name, "kube-proxy.conf" | "kubelet" | "kubelet.conf")) {
                println!("MISSING! {}", h.long_name);
            }
            continue;
        }
        if d.ca(h.ca_name).is_err() {
            println!("Detected external {}, {} can't be renewed", h.ca_name, h.long_name);
            continue;
        }
        d.renew(h)?;
        println!("{} renewed", h.long_name);
    }
    if which == "all" {
        println!(
            "\nDone renewing certificates. You must restart the kube-apiserver, kube-controller-manager, kube-scheduler and etcd, so that they can use the new certificates."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renew_keeps_identity() {
        let dir = std::env::temp_dir().join(format!("kubeadm-renew-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pki = dir.join("pki");
        let o = super::super::certs::CertOptions {
            cert_dir: pki.clone(),
            advertise_address: "10.0.0.1".parse().unwrap(),
            service_cluster_ip: "10.96.0.1".parse().unwrap(),
            dns_domain: "cluster.local".into(),
            extra_sans: vec![],
            node_name: "n1".into(),
            node_ips: vec![],
        };
        let ca = super::super::certs::all(&o).unwrap();
        super::super::kubeconfig::all(&dir, &ca, "https://10.0.0.1:6443", "kubernetes", "n1").unwrap();
        let d = Dirs { cert_dir: pki.clone(), kubeconfig_dir: dir.clone() };
        let before = std::fs::read(pki.join("apiserver.crt")).unwrap();
        let admin_before = d.read(&handlers()[0]).unwrap().unwrap();
        renew(&d, "all").unwrap();
        let after = std::fs::read(pki.join("apiserver.crt")).unwrap();
        assert_ne!(before, after);
        let der = first_cert(&after).unwrap();
        let (_, a) = x509_parser::parse_x509_certificate(&der).unwrap();
        assert_eq!(a.subject().iter_common_name().next().unwrap().as_str().unwrap(), "kube-apiserver");
        assert!(a.subject_alternative_name().unwrap().unwrap().value.general_names.len() >= 6);
        let admin_after = d.read(&handlers()[0]).unwrap().unwrap();
        assert_ne!(admin_before, admin_after);
        // The renewed kubeconfig still works.
        let cfg = k8s_client_go::tools::clientcmd::load_config(&dir.join("admin.conf"), None).unwrap();
        cfg.tls.rustls().unwrap();
        let table = check_expiration(&d).unwrap();
        assert!(table.starts_with("CERTIFICATE                EXPIRES"), "{table}");
        assert!(
            table.contains("apiserver-etcd-client") && table.contains("etcd-ca") && (table.contains("364d") || table.contains("365d")),
            "{table}"
        );
        std::fs::remove_dir_all(dir).ok();
    }
}
