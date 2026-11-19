//! Port of `pkg/controller/certificates/authority`: a CA that signs
//! certificate requests under a [`SigningPolicy`] (`PermissiveSigningPolicy`).
//!
//! The certificate is built like Go's `x509.CreateCertificate` with the
//! signer's template: subject, public key and subjectAltNames come unchanged
//! from the request; the requested extensions are otherwise dropped; key
//! usages come from `spec.usages`; `basicConstraints` says "not a CA"; the
//! authority key identifier is the CA's subject key identifier.

use std::time::Duration;

use chrono::{DateTime, Datelike, Utc};
use kube_apis::certificates::ParsedCsr;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{self, EcdsaKeyPair, Ed25519KeyPair, RsaKeyPair};
use x509_parser::pem::Pem;
use yasna::models::ObjectIdentifier;
use yasna::{DERWriter, Tag};

enum Key {
    Ecdsa(EcdsaKeyPair, &'static [u64]),
    Rsa(RsaKeyPair),
    Ed25519(Ed25519KeyPair),
}

const ECDSA_SHA256: &[u64] = &[1, 2, 840, 10045, 4, 3, 2];
const ECDSA_SHA384: &[u64] = &[1, 2, 840, 10045, 4, 3, 3];
const RSA_SHA256: &[u64] = &[1, 2, 840, 113549, 1, 1, 11];
const ED25519: &[u64] = &[1, 3, 101, 112];

/// A loaded CA certificate and key (`CertificateAuthority`).
pub struct CertificateAuthority {
    /// PEM of the CA certificate.
    pub cert_pem: Vec<u8>,
    issuer_der: Vec<u8>,
    subject_key_id: Option<Vec<u8>>,
    /// `Certificate.NotAfter`, unix seconds.
    pub not_after: i64,
    key: Key,
    rng: SystemRandom,
}

/// `PermissiveSigningPolicy`
pub struct SigningPolicy {
    pub ttl: Duration,
    pub usages: Vec<String>,
    pub backdate: Duration,
    pub short: Duration,
    pub now: DateTime<Utc>,
}

fn first_block(pem: &[u8], what: &str) -> Result<Pem, String> {
    Pem::iter_from_buffer(pem).next().and_then(Result::ok).ok_or_else(|| format!("data does not contain any valid {what}"))
}

impl CertificateAuthority {
    /// Loads a CA from PEM files' contents (`newCAProvider` → `setCA`).
    pub fn load(cert_pem: &[u8], key_pem: &[u8]) -> Result<Self, String> {
        let block = first_block(cert_pem, "RSA or ECDSA certificates")?;
        let cert = block.parse_x509().map_err(|e| format!("error reading CA cert file: {e}"))?;
        let tbs = &cert.tbs_certificate;
        let subject_key_id = tbs.extensions().iter().find_map(|e| match e.parsed_extension() {
            x509_parser::extensions::ParsedExtension::SubjectKeyIdentifier(k) => Some(k.0.to_vec()),
            _ => None,
        });
        let not_after = tbs.validity.not_after.timestamp();
        let issuer_der = tbs.subject.as_raw().to_vec();
        let rng = SystemRandom::new();
        let k = first_block(key_pem, "private key")?;
        let der = k.contents.as_slice();
        let key = match k.label.as_str() {
            "PRIVATE KEY" => EcdsaKeyPair::from_pkcs8(&signature::ECDSA_P256_SHA256_ASN1_SIGNING, der, &rng)
                .map(|kp| Key::Ecdsa(kp, ECDSA_SHA256))
                .or_else(|_| {
                    EcdsaKeyPair::from_pkcs8(&signature::ECDSA_P384_SHA384_ASN1_SIGNING, der, &rng).map(|kp| Key::Ecdsa(kp, ECDSA_SHA384))
                })
                .or_else(|_| RsaKeyPair::from_pkcs8(der).map(Key::Rsa))
                .or_else(|_| Ed25519KeyPair::from_pkcs8_maybe_unchecked(der).map(Key::Ed25519))
                .map_err(|e| format!("error reading CA key file: {e}"))?,
            "RSA PRIVATE KEY" => Key::Rsa(RsaKeyPair::from_der(der).map_err(|e| format!("error reading CA key file: {e}"))?),
            other => return Err(format!("error reading CA key file: unsupported private key type {other:?} (want PKCS#8 or PKCS#1)")),
        };
        Ok(Self { cert_pem: cert_pem.to_vec(), issuer_der, subject_key_id, not_after, key, rng })
    }

    fn sig_alg(&self) -> &'static [u64] {
        match &self.key {
            Key::Ecdsa(_, oid) => oid,
            Key::Rsa(_) => RSA_SHA256,
            Key::Ed25519(_) => ED25519,
        }
    }

    fn write_sig_alg(&self, w: DERWriter) {
        let oid = self.sig_alg();
        w.write_sequence(|w| {
            w.next().write_oid(&ObjectIdentifier::from_slice(oid));
            if oid == RSA_SHA256 {
                w.next().write_null();
            }
        });
    }

    fn sign_bytes(&self, msg: &[u8]) -> Result<Vec<u8>, String> {
        match &self.key {
            Key::Ecdsa(k, _) => k.sign(&self.rng, msg).map(|s| s.as_ref().to_vec()).map_err(|e| e.to_string()),
            Key::Rsa(k) => {
                let mut sig = vec![0; k.public().modulus_len()];
                k.sign(&signature::RSA_PKCS1_SHA256, &self.rng, msg, &mut sig).map_err(|e| e.to_string())?;
                Ok(sig)
            }
            Key::Ed25519(k) => Ok(k.sign(msg).as_ref().to_vec()),
        }
    }

    /// `Sign`: returns the DER certificate.
    pub fn sign(&self, req: &ParsedCsr, policy: &SigningPolicy) -> Result<Vec<u8>, String> {
        let (key_usage, ext_usages) = key_usages_from_strings(&policy.usages)?;
        let (not_before, not_after) = policy.apply(self.not_after)?;
        let mut serial = [0u8; 16];
        self.rng.fill(&mut serial).map_err(|_| format!("unable to generate a serial number for {}", req.common_name))?;
        let tbs = yasna::construct_der(|w| {
            w.write_sequence(|w| {
                w.next().write_tagged(Tag::context(0), |w| w.write_u8(2));
                w.next().write_bigint_bytes(&serial, true);
                self.write_sig_alg(w.next());
                w.next().write_der(&self.issuer_der);
                w.next().write_sequence(|w| {
                    write_time(w.next(), not_before);
                    write_time(w.next(), not_after);
                });
                w.next().write_der(&req.subject_der);
                w.next().write_der(&req.spki_der);
                w.next().write_tagged(Tag::context(3), |w| {
                    w.write_sequence(|w| {
                        if key_usage != 0 {
                            write_ext(w.next(), &[2, 5, 29, 15], true, &key_usage_der(key_usage));
                        }
                        if !ext_usages.is_empty() {
                            let v = yasna::construct_der(|w| {
                                w.write_sequence(|w| {
                                    for oid in &ext_usages {
                                        w.next().write_oid(&ObjectIdentifier::from_slice(oid));
                                    }
                                })
                            });
                            write_ext(w.next(), &[2, 5, 29, 37], false, &v);
                        }
                        // basicConstraints: CA:FALSE
                        write_ext(w.next(), &[2, 5, 29, 19], true, &yasna::construct_der(|w| w.write_sequence(|_| {})));
                        if let Some(ski) = &self.subject_key_id {
                            let v = yasna::construct_der(|w| {
                                w.write_sequence(|w| w.next().write_tagged_implicit(Tag::context(0), |w| w.write_bytes(ski)))
                            });
                            write_ext(w.next(), &[2, 5, 29, 35], false, &v);
                        }
                        if let Some(san) = &req.san_der {
                            // Critical when the subject is empty (RFC 5280 4.2.1.6).
                            let empty_subject = req.subject_der == [0x30, 0x00];
                            write_ext(w.next(), &[2, 5, 29, 17], empty_subject, san);
                        }
                    })
                });
            })
        });
        let sig = self.sign_bytes(&tbs)?;
        Ok(yasna::construct_der(|w| {
            w.write_sequence(|w| {
                w.next().write_der(&tbs);
                self.write_sig_alg(w.next());
                w.next().write_bitvec_bytes(&sig, sig.len() * 8);
            })
        }))
    }
}

impl SigningPolicy {
    /// `PermissiveSigningPolicy.apply`: (notBefore, notAfter).
    fn apply(&self, signer_not_after: i64) -> Result<(DateTime<Utc>, DateTime<Utc>), String> {
        let now = self.now;
        let backdate = chrono::Duration::from_std(self.backdate).unwrap_or_default();
        let ttl = chrono::Duration::from_std(self.ttl).unwrap_or_default();
        let not_before = now - backdate;
        let mut not_after = if self.ttl < self.short { now + ttl } else { now + ttl - backdate };
        let signer = DateTime::<Utc>::from_timestamp(signer_not_after, 0).unwrap_or(now);
        if not_after >= signer {
            not_after = signer;
        }
        if not_before >= signer {
            return Err(format!("the signer has expired: NotAfter={}", go_time(signer)));
        }
        if now >= signer {
            return Err(format!("refusing to sign a certificate that expired in the past: NotAfter={}", go_time(signer)));
        }
        Ok((not_before, not_after))
    }
}

fn go_time(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M:%S +0000 UTC").to_string()
}

/// UTCTime for 1950–2049, GeneralizedTime otherwise (RFC 5280 4.1.2.5).
fn write_time(w: DERWriter, t: DateTime<Utc>) {
    let (tag, s) = if (1950..2050).contains(&t.year()) {
        (0x17u8, t.format("%y%m%d%H%M%SZ").to_string())
    } else {
        (0x18u8, t.format("%Y%m%d%H%M%SZ").to_string())
    };
    let mut der = vec![tag, s.len() as u8];
    der.extend_from_slice(s.as_bytes());
    w.write_der(&der);
}

fn write_ext(w: DERWriter, oid: &[u64], critical: bool, value: &[u8]) {
    w.write_sequence(|w| {
        w.next().write_oid(&ObjectIdentifier::from_slice(oid));
        if critical {
            w.next().write_bool(true);
        }
        w.next().write_bytes(value);
    });
}

/// Go's `x509.KeyUsage` bits (`KeyUsageDigitalSignature` = bit 0 ...).
fn key_usage_bit(u: &str) -> Option<u16> {
    Some(match u {
        "signing" | "digital signature" => 0,
        "content commitment" => 1,
        "key encipherment" => 2,
        "data encipherment" => 3,
        "key agreement" => 4,
        "cert sign" => 5,
        "crl sign" => 6,
        "encipher only" => 7,
        "decipher only" => 8,
        _ => return None,
    })
}

/// `extKeyUsageDict`, with Go's `x509.ExtKeyUsage` enum order for sorting.
fn ext_key_usage(u: &str) -> Option<(u8, &'static [u64])> {
    Some(match u {
        "any" => (0, &[2, 5, 29, 37, 0]),
        "server auth" => (1, &[1, 3, 6, 1, 5, 5, 7, 3, 1]),
        "client auth" => (2, &[1, 3, 6, 1, 5, 5, 7, 3, 2]),
        "code signing" => (3, &[1, 3, 6, 1, 5, 5, 7, 3, 3]),
        "email protection" | "s/mime" => (4, &[1, 3, 6, 1, 5, 5, 7, 3, 4]),
        "ipsec end system" => (5, &[1, 3, 6, 1, 5, 5, 7, 3, 5]),
        "ipsec tunnel" => (6, &[1, 3, 6, 1, 5, 5, 7, 3, 6]),
        "ipsec user" => (7, &[1, 3, 6, 1, 5, 5, 7, 3, 7]),
        "timestamping" => (8, &[1, 3, 6, 1, 5, 5, 7, 3, 8]),
        "ocsp signing" => (9, &[1, 3, 6, 1, 5, 5, 7, 3, 9]),
        "microsoft sgc" => (10, &[1, 3, 6, 1, 4, 1, 311, 10, 3, 3]),
        "netscape sgc" => (11, &[2, 16, 840, 1, 113730, 4, 1]),
        _ => return None,
    })
}

/// `keyUsagesFromStrings`
fn key_usages_from_strings(usages: &[String]) -> Result<(u16, Vec<&'static [u64]>), String> {
    let mut ku = 0u16;
    let mut ext = std::collections::BTreeMap::new();
    let mut unrecognized = vec![];
    for u in usages {
        if let Some(b) = key_usage_bit(u) {
            ku |= 1 << b;
        } else if let Some((order, oid)) = ext_key_usage(u) {
            ext.insert(order, oid);
        } else {
            unrecognized.push(format!("{u:?}"));
        }
    }
    if !unrecognized.is_empty() {
        return Err(format!("unrecognized usage values: [{}]", unrecognized.join(" ")));
    }
    Ok((ku, ext.into_values().collect()))
}

/// `marshalKeyUsage`: bit 0 is the most significant bit of the first byte;
/// trailing zero bits are not encoded.
fn key_usage_der(ku: u16) -> Vec<u8> {
    let bytes = [(ku as u8).reverse_bits(), ((ku >> 8) as u8).reverse_bits()];
    let len = if bytes[1] != 0 { 2 } else { 1 };
    let bits = &bytes[..len];
    let trailing = bits[len - 1].trailing_zeros() as usize;
    yasna::construct_der(|w| w.write_bitvec_bytes(bits, len * 8 - trailing))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ca() -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut p = rcgen::CertificateParams::default();
        p.distinguished_name.push(rcgen::DnType::CommonName, "kubernetes");
        p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        p.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign];
        p.not_after = rcgen::date_time_ymd(2099, 1, 1);
        (p.self_signed(&key).unwrap().pem(), key.serialize_pem())
    }

    #[test]
    fn signs_like_go() {
        let (cert, key) = ca();
        let ca = CertificateAuthority::load(cert.as_bytes(), key.as_bytes()).unwrap();
        // Several organizations and a SAN survive unchanged.
        let rk = rcgen::KeyPair::generate().unwrap();
        let mut p = rcgen::CertificateParams::new(vec!["node1".to_string()]).unwrap();
        p.distinguished_name.push(rcgen::DnType::OrganizationName, "system:nodes");
        p.distinguished_name.push(rcgen::DnType::CommonName, "system:node:node1");
        let pem = p.serialize_request(&rk).unwrap().pem().unwrap();
        let req = kube_apis::certificates::parse_csr(pem.as_bytes()).unwrap();
        let now = Utc::now();
        let policy = SigningPolicy {
            ttl: Duration::from_secs(3600),
            usages: vec!["digital signature".into(), "key encipherment".into(), "server auth".into()],
            backdate: Duration::from_secs(300),
            short: Duration::from_secs(8 * 3600),
            now,
        };
        let der = ca.sign(&req, &policy).unwrap();
        let (_, c) = x509_parser::parse_x509_certificate(&der).unwrap();
        // Verifies with the CA's key.
        let ca_block = first_block(cert.as_bytes(), "x").unwrap();
        let ca_cert = ca_block.parse_x509().unwrap();
        c.verify_signature(Some(ca_cert.public_key())).unwrap();
        assert_eq!(c.subject().as_raw(), req.subject_der.as_slice(), "subject copied unchanged");
        assert!(c.subject().to_string().contains("O=system:nodes"));
        assert_eq!(c.issuer().to_string(), "CN=kubernetes");
        assert_eq!(c.validity().not_before.timestamp(), now.timestamp() - 300);
        assert_eq!(c.validity().not_after.timestamp(), now.timestamp() + 3600, "short certs are not shortened by the backdate");
        let ku = c.key_usage().unwrap().unwrap();
        assert!(ku.critical && ku.value.digital_signature() && ku.value.key_encipherment() && !ku.value.key_cert_sign());
        let eku = c.extended_key_usage().unwrap().unwrap();
        assert!(eku.value.server_auth && !eku.value.client_auth);
        assert!(!c.basic_constraints().unwrap().unwrap().value.ca);
        assert_eq!(c.subject_alternative_name().unwrap().unwrap().value.general_names.len(), 1);
        // Long certificates lose the backdate; nothing outlives the CA.
        let long = SigningPolicy { ttl: Duration::from_secs(400 * 365 * 86400), ..policy };
        let der = ca.sign(&req, &long).unwrap();
        let (_, c) = x509_parser::parse_x509_certificate(&der).unwrap();
        assert_eq!(c.validity().not_after.timestamp(), ca.not_after);
        assert_eq!(key_usages_from_strings(&["bogus".into()]).unwrap_err(), "unrecognized usage values: [\"bogus\"]");
    }

    #[test]
    fn key_usage_encoding() {
        // digitalSignature|keyEncipherment = 0b101 → 0xa0 with 5 unused bits.
        assert_eq!(key_usage_der(0b101), vec![0x03, 0x02, 0x05, 0xa0]);
        // decipherOnly (bit 8) needs two bytes.
        assert_eq!(key_usage_der(1 << 8), vec![0x03, 0x03, 0x07, 0x00, 0x80]);
    }
}
