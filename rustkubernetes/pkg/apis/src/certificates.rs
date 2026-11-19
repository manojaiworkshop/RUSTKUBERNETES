//! Port of `pkg/apis/certificates` (`helpers.go`) and the parts of
//! `validation.go` shared by the registry, admission and the controllers.

use std::collections::BTreeSet;
use std::net::IpAddr;

use k8s_apimachinery::validation::{is_dns1123_label, is_dns1123_subdomain, ErrorList, FieldError};
use x509_parser::certification_request::X509CertificationRequest;
use x509_parser::extensions::{GeneralName, ParsedExtension};
use x509_parser::pem::Pem;
use x509_parser::prelude::FromDer;

/// Public key algorithm of a request (`x509.PublicKeyAlgorithm` + curve).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyAlgorithm {
    Rsa,
    EcdsaP256,
    EcdsaP384,
    Ed25519,
    Other(String),
}

/// The parts of an `x509.CertificateRequest` Kubernetes looks at.
#[derive(Clone, Debug)]
pub struct ParsedCsr {
    pub common_name: String,
    pub organizations: Vec<String>,
    pub dns_names: Vec<String>,
    pub ip_addresses: Vec<IpAddr>,
    pub email_addresses: Vec<String>,
    pub uris: Vec<String>,
    pub key_algorithm: KeyAlgorithm,
    /// `subjectPublicKey` bits (what goes into the issued certificate's SPKI).
    pub public_key: Vec<u8>,
    /// The whole DER request.
    pub der: Vec<u8>,
    /// DER `Name` of the subject, copied unchanged into issued certificates.
    pub subject_der: Vec<u8>,
    /// DER `SubjectPublicKeyInfo`.
    pub spki_der: Vec<u8>,
    /// Value (`extnValue` contents) of a requested subjectAltName extension.
    pub san_der: Option<Vec<u8>>,
}

/// First PEM block of `data` (`pem.Decode` skips anything before it).
fn first_pem(data: &[u8]) -> Option<Pem> {
    Pem::iter_from_buffer(data).next().and_then(Result::ok)
}

/// `ParseCSR` + `CheckSignature`.
pub fn parse_csr(pem: &[u8]) -> Result<ParsedCsr, String> {
    let block = first_pem(pem).filter(|b| b.label == "CERTIFICATE REQUEST").ok_or("PEM block type must be CERTIFICATE REQUEST")?;
    parse_csr_der(&block.contents)
}

pub fn parse_csr_der(der: &[u8]) -> Result<ParsedCsr, String> {
    let (_, csr) = X509CertificationRequest::from_der(der).map_err(|e| format!("x509: malformed certificate request: {e}"))?;
    csr.verify_signature().map_err(|e| format!("x509: invalid signature: {e}"))?;
    let info = &csr.certification_request_info;
    let attr_strings = |it: &mut dyn Iterator<Item = &x509_parser::x509::AttributeTypeAndValue>| -> Vec<String> {
        it.filter_map(|a| a.as_str().ok().map(str::to_string)).collect()
    };
    let common_name = attr_strings(&mut info.subject.iter_common_name()).into_iter().next().unwrap_or_default();
    let organizations = attr_strings(&mut info.subject.iter_organization());
    let mut out = ParsedCsr {
        common_name,
        organizations,
        dns_names: vec![],
        ip_addresses: vec![],
        email_addresses: vec![],
        uris: vec![],
        key_algorithm: key_algorithm(&info.subject_pki),
        public_key: info.subject_pki.subject_public_key.data.to_vec(),
        der: der.to_vec(),
        subject_der: info.subject.as_raw().to_vec(),
        spki_der: info.subject_pki.raw.to_vec(),
        san_der: None,
    };
    for attr in info.attributes() {
        if let x509_parser::cri_attributes::ParsedCriAttribute::ExtensionRequest(req) = attr.parsed_attribute() {
            for ext in &req.extensions {
                if ext.oid.to_id_string() == "2.5.29.17" {
                    out.san_der = Some(ext.value.to_vec());
                }
            }
        }
    }
    if let Some(exts) = csr.requested_extensions() {
        for ext in exts {
            if let ParsedExtension::SubjectAlternativeName(san) = ext {
                for n in &san.general_names {
                    match n {
                        GeneralName::DNSName(d) => out.dns_names.push(d.to_string()),
                        GeneralName::RFC822Name(e) => out.email_addresses.push(e.to_string()),
                        GeneralName::URI(u) => out.uris.push(u.to_string()),
                        GeneralName::IPAddress(b) => match b.len() {
                            4 => out.ip_addresses.push(IpAddr::from(<[u8; 4]>::try_from(*b).unwrap())),
                            16 => out.ip_addresses.push(IpAddr::from(<[u8; 16]>::try_from(*b).unwrap())),
                            _ => return Err("x509: certificate request contained IP address of length".into()),
                        },
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(out)
}

fn key_algorithm(spki: &x509_parser::x509::SubjectPublicKeyInfo) -> KeyAlgorithm {
    let oid = spki.algorithm.algorithm.to_id_string();
    match oid.as_str() {
        "1.2.840.113549.1.1.1" => KeyAlgorithm::Rsa,
        "1.3.101.112" => KeyAlgorithm::Ed25519,
        "1.2.840.10045.2.1" => {
            let curve = spki.algorithm.parameters.as_ref().and_then(|p| p.as_oid().ok()).map(|o| o.to_id_string()).unwrap_or_default();
            match curve.as_str() {
                "1.2.840.10045.3.1.7" => KeyAlgorithm::EcdsaP256,
                "1.3.132.0.34" => KeyAlgorithm::EcdsaP384,
                c => KeyAlgorithm::Other(format!("ecdsa {c}")),
            }
        }
        o => KeyAlgorithm::Other(o.to_string()),
    }
}

/// `validateCertificate`: zero or more `CERTIFICATE` blocks, each parseable.
pub fn validate_certificate_pem(data: &[u8]) -> Result<(), String> {
    if data.is_empty() {
        return Ok(());
    }
    let mut blocks = 0;
    for b in Pem::iter_from_buffer(data) {
        let b = b.map_err(|e| e.to_string())?;
        if b.label != "CERTIFICATE" {
            return Err(format!("only CERTIFICATE PEM blocks are allowed, found {:?}", b.label));
        }
        b.parse_x509().map_err(|e| format!("x509: malformed certificate: {e}"))?;
        blocks += 1;
    }
    if blocks == 0 {
        return Err("must contain at least one CERTIFICATE PEM block".into());
    }
    Ok(())
}

/// `NotAfter` of the first certificate in a PEM bundle (unix seconds).
pub fn certificate_not_after(data: &[u8]) -> Option<i64> {
    let b = first_pem(data)?;
    let c = b.parse_x509().ok()?;
    Some(c.validity().not_after.timestamp())
}

/// `allValidUsages`, sorted like `sets.String.List()`.
pub const ALL_VALID_USAGES: &[&str] = &[
    "any",
    "cert sign",
    "client auth",
    "code signing",
    "content commitment",
    "crl sign",
    "data encipherment",
    "decipher only",
    "digital signature",
    "email protection",
    "encipher only",
    "ipsec end system",
    "ipsec tunnel",
    "ipsec user",
    "key agreement",
    "key encipherment",
    "microsoft sgc",
    "netscape sgc",
    "ocsp signing",
    "s/mime",
    "server auth",
    "signing",
    "timestamping",
];

/// `ValidateSignerName` (`pkg/apis/core/validation/names.go`).
pub fn validate_signer_name(path: &str, signer: &str) -> ErrorList {
    let mut el = vec![];
    if signer.is_empty() {
        el.push(FieldError::required(path, ""));
        return el;
    }
    let segments: Vec<&str> = signer.split('/').collect();
    if segments.len() != 2 {
        el.push(FieldError::invalid(path, signer, "must be a fully qualified domain and path of the form 'example.com/signer-name'"));
        return el;
    }
    const MAX_DOMAIN: usize = 253;
    if segments[0].len() > MAX_DOMAIN {
        el.push(FieldError::too_long(path, MAX_DOMAIN));
    }
    let domain_labels: Vec<&str> = segments[0].split('.').collect();
    for lbl in &domain_labels {
        let errs = is_dns1123_label(lbl);
        if !errs.is_empty() {
            for e in errs {
                el.push(FieldError::invalid(path, segments[0], format!("validating label {lbl:?}: {e}")));
            }
            break;
        }
    }
    if domain_labels.len() < 2 {
        el.push(FieldError::invalid(path, segments[0], "should be a domain with at least two segments separated by dots"));
    }
    for lbl in segments[1].split('.') {
        let errs = is_dns1123_subdomain(lbl);
        if !errs.is_empty() {
            for e in errs {
                el.push(FieldError::invalid(path, segments[1], format!("validating label {lbl:?}: {e}")));
            }
            break;
        }
    }
    let max = MAX_DOMAIN + (253 + 63 + 1) + 1;
    if signer.len() > max {
        el.push(FieldError::too_long(path, max));
    }
    el
}

fn usage_set(usages: &[String]) -> BTreeSet<&str> {
    usages.iter().map(String::as_str).collect()
}

fn set(v: &[&'static str]) -> BTreeSet<&'static str> {
    v.iter().copied().collect()
}

/// `ValidateKubeletServingCSR`
pub fn validate_kubelet_serving_csr(req: &ParsedCsr, usages: &[String]) -> Result<(), String> {
    if req.organizations != ["system:nodes"] {
        return Err("subject organization is not system:nodes".into());
    }
    if req.dns_names.is_empty() && req.ip_addresses.is_empty() {
        return Err("DNS or IP subjectAltName is required".into());
    }
    if !req.email_addresses.is_empty() {
        return Err("Email subjectAltNames are not allowed".into());
    }
    if !req.uris.is_empty() {
        return Err("URI subjectAltNames are not allowed".into());
    }
    let u = usage_set(usages);
    let required = set(&["digital signature", "key encipherment", "server auth"]);
    if u != required && u != set(&["digital signature", "server auth"]) {
        return Err(format!("usages did not match [{}]", required.into_iter().collect::<Vec<_>>().join(" ")));
    }
    if !req.common_name.starts_with("system:node:") {
        return Err("subject common name does not begin with system:node:".into());
    }
    Ok(())
}

/// `ValidateKubeletClientCSR`
pub fn validate_kubelet_client_csr(req: &ParsedCsr, usages: &[String]) -> Result<(), String> {
    if req.organizations != ["system:nodes"] {
        return Err("subject organization is not system:nodes".into());
    }
    if !req.dns_names.is_empty() {
        return Err("DNS subjectAltNames are not allowed".into());
    }
    if !req.email_addresses.is_empty() {
        return Err("Email subjectAltNames are not allowed".into());
    }
    if !req.ip_addresses.is_empty() {
        return Err("IP subjectAltNames are not allowed".into());
    }
    if !req.uris.is_empty() {
        return Err("URI subjectAltNames are not allowed".into());
    }
    if !req.common_name.starts_with("system:node:") {
        return Err("subject common name does not begin with system:node:".into());
    }
    let u = usage_set(usages);
    let required = set(&["client auth", "digital signature", "key encipherment"]);
    if u != required && u != set(&["client auth", "digital signature"]) {
        return Err(format!("usages did not match [{}]", required.into_iter().collect::<Vec<_>>().join(" ")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn csr(cn: &str, orgs: &[&str], dns: &[&str]) -> String {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut p = rcgen::CertificateParams::new(dns.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
        let mut dn = rcgen::DistinguishedName::new();
        for o in orgs {
            dn.push(rcgen::DnType::OrganizationName, *o);
        }
        dn.push(rcgen::DnType::CommonName, cn);
        p.distinguished_name = dn;
        p.serialize_request(&key).unwrap().pem().unwrap()
    }

    #[test]
    fn parse_and_kubelet_rules() {
        let pem = csr("system:node:n1", &["system:nodes"], &[]);
        let r = parse_csr(pem.as_bytes()).unwrap();
        assert_eq!(r.common_name, "system:node:n1");
        assert_eq!(r.organizations, ["system:nodes"]);
        assert_eq!(r.key_algorithm, KeyAlgorithm::EcdsaP256);
        let u = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(validate_kubelet_client_csr(&r, &u(&["digital signature", "client auth"])).is_ok());
        assert!(validate_kubelet_client_csr(&r, &u(&["digital signature", "key encipherment", "client auth"])).is_ok());
        assert_eq!(
            validate_kubelet_client_csr(&r, &u(&["server auth"])).unwrap_err(),
            "usages did not match [client auth digital signature key encipherment]"
        );
        assert_eq!(
            validate_kubelet_serving_csr(&r, &u(&["digital signature", "server auth"])).unwrap_err(),
            "DNS or IP subjectAltName is required"
        );
        let serving = parse_csr(csr("system:node:n1", &["system:nodes"], &["n1"]).as_bytes()).unwrap();
        assert!(validate_kubelet_serving_csr(&serving, &u(&["digital signature", "key encipherment", "server auth"])).is_ok());
        assert_eq!(validate_kubelet_client_csr(&serving, &u(&["client auth"])).unwrap_err(), "DNS subjectAltNames are not allowed");
        let other = parse_csr(csr("bob", &["devs"], &[]).as_bytes()).unwrap();
        assert_eq!(validate_kubelet_client_csr(&other, &[]).unwrap_err(), "subject organization is not system:nodes");
        assert_eq!(parse_csr(b"junk").unwrap_err(), "PEM block type must be CERTIFICATE REQUEST");
    }

    #[test]
    fn signer_names() {
        assert!(validate_signer_name("spec.signerName", "kubernetes.io/kube-apiserver-client").is_empty());
        assert!(validate_signer_name("spec.signerName", "example.com/a.b").is_empty());
        assert!(validate_signer_name("spec.signerName", "foo")[0].message().contains("example.com/signer-name"));
        assert!(validate_signer_name("spec.signerName", "foo/bar")[0].message().contains("at least two segments"));
        assert_eq!(validate_signer_name("spec.signerName", "")[0].field, "spec.signerName");
    }
}
