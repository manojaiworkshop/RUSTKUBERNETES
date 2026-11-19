//! Value transformers: encryption at rest (`k8s.io/apiserver/pkg/storage/value`,
//! `pkg/server/options/encryptionconfig`).
//!
//! Configured with an `EncryptionConfiguration` (`--encryption-provider-config`):
//!
//! ```yaml
//! apiVersion: apiserver.config.k8s.io/v1
//! kind: EncryptionConfiguration
//! resources:
//!   - resources: [secrets]
//!     providers:
//!       - aesgcm:
//!           keys:
//!             - name: key1
//!               secret: <base64 of 16 or 32 random bytes>
//!       - identity: {}
//! ```
//!
//! Writes use the first provider; reads pick the provider by the stored prefix
//! (`k8s:enc:aesgcm:v1:<key name>:`), so keys can be rotated by adding a new
//! first key. AES-GCM values are `prefix || nonce(12) || ciphertext+tag` with the
//! etcd key as additional authenticated data, exactly like upstream. Resources not
//! listed are stored as they are.

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_128_GCM, AES_256_GCM};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;

#[derive(Deserialize)]
struct Config {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    resources: Vec<ResourceConfig>,
}

#[derive(Deserialize)]
struct ResourceConfig {
    resources: Vec<String>,
    providers: Vec<serde_yaml::Value>,
}

#[derive(Deserialize)]
struct Key {
    name: String,
    secret: String,
}

enum Provider {
    Identity,
    AesGcm(Vec<(String, LessSafeKey)>),
}

impl Provider {
    fn prefix_of(name: &str) -> String {
        format!("k8s:enc:aesgcm:v1:{name}:")
    }
}

struct Rule {
    /// Resource names (`secrets`, `configmaps`; `*` / `*.*` = everything).
    resources: Vec<String>,
    providers: Vec<Provider>,
}

pub struct Transformer {
    rules: Vec<Rule>,
}

/// The resource of an API storage key (`/registry/<resource>/...`, upstream layout).
pub fn resource_of_key(api_key: &str) -> &str {
    let rest = api_key.strip_prefix("/registry/").unwrap_or(api_key);
    let mut it = rest.split('/');
    match (it.next().unwrap_or(""), it.next()) {
        ("minions", _) => "nodes",
        ("services", Some("specs")) => "services",
        ("services", Some("endpoints")) => "endpoints",
        ("controllers", _) => "replicationcontrollers",
        (r, _) => r,
    }
}

impl Transformer {
    pub fn from_yaml(yaml: &str) -> Result<Self, String> {
        let cfg: Config = serde_yaml::from_str(yaml).map_err(|e| format!("invalid EncryptionConfiguration: {e}"))?;
        if !cfg.kind.is_empty() && cfg.kind != "EncryptionConfiguration" {
            return Err(format!("expected kind EncryptionConfiguration, got {}", cfg.kind));
        }
        let mut rules = vec![];
        for rc in cfg.resources {
            let mut providers = vec![];
            for p in &rc.providers {
                let m = p.as_mapping().ok_or("each provider must be a map")?;
                let (name, body) = m.iter().next().ok_or("empty provider")?;
                match name.as_str().unwrap_or("") {
                    "identity" => providers.push(Provider::Identity),
                    "aesgcm" => {
                        let keys: Vec<Key> = serde_yaml::from_value(body["keys"].clone()).map_err(|e| format!("aesgcm keys: {e}"))?;
                        if keys.is_empty() {
                            return Err("aesgcm needs at least one key".into());
                        }
                        let mut out = vec![];
                        for k in keys {
                            use base64::Engine;
                            let raw = base64::engine::general_purpose::STANDARD
                                .decode(k.secret.trim())
                                .map_err(|e| format!("key {}: invalid base64: {e}", k.name))?;
                            let alg = match raw.len() {
                                16 => &AES_128_GCM,
                                32 => &AES_256_GCM,
                                n => return Err(format!("key {}: AES-GCM keys must be 16 or 32 bytes, got {n}", k.name)),
                            };
                            out.push((k.name, LessSafeKey::new(UnboundKey::new(alg, &raw).map_err(|_| "invalid AES key")?)));
                        }
                        providers.push(Provider::AesGcm(out));
                    }
                    other => return Err(format!("encryption provider {other:?} is not supported (use aesgcm or identity)")),
                }
            }
            if providers.is_empty() {
                return Err("every resource entry needs at least one provider".into());
            }
            let resources = rc.resources.iter().map(|r| r.split('.').next().unwrap_or(r).to_string()).collect();
            rules.push(Rule { resources, providers });
        }
        Ok(Self { rules })
    }

    fn rule(&self, resource: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| r.resources.iter().any(|x| x == resource || x == "*"))
    }

    /// Encrypts `data` stored under `etcd_key` (resource taken from `api_key`).
    pub fn to_storage(&self, data: &[u8], etcd_key: &[u8], api_key: &str) -> Result<Vec<u8>, String> {
        let Some(rule) = self.rule(resource_of_key(api_key)) else { return Ok(data.to_vec()) };
        match &rule.providers[0] {
            Provider::Identity => Ok(data.to_vec()),
            Provider::AesGcm(keys) => {
                let (name, key) = &keys[0];
                let mut nonce = [0u8; 12];
                SystemRandom::new().fill(&mut nonce).map_err(|_| "no randomness")?;
                let mut buf = data.to_vec();
                key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(etcd_key), &mut buf)
                    .map_err(|_| "encryption failed")?;
                let mut out = Provider::prefix_of(name).into_bytes();
                out.extend_from_slice(&nonce);
                out.extend_from_slice(&buf);
                Ok(out)
            }
        }
    }

    /// Decrypts a stored value, choosing the provider from its prefix.
    pub fn from_storage(&self, data: &[u8], etcd_key: &[u8], api_key: &str) -> Result<Vec<u8>, String> {
        let Some(rule) = self.rule(resource_of_key(api_key)) else { return Ok(data.to_vec()) };
        for p in &rule.providers {
            match p {
                Provider::AesGcm(keys) => {
                    for (name, key) in keys {
                        let prefix = Provider::prefix_of(name);
                        if let Some(rest) = data.strip_prefix(prefix.as_bytes()) {
                            if rest.len() < 12 + 16 {
                                return Err("the stored value is too short".into());
                            }
                            let (nonce, ct) = rest.split_at(12);
                            let mut buf = ct.to_vec();
                            let n = Nonce::try_assume_unique_for_key(nonce).map_err(|_| "bad nonce")?;
                            let plain = key
                                .open_in_place(n, Aad::from(etcd_key), &mut buf)
                                .map_err(|_| format!("decryption with key {name} failed"))?;
                            return Ok(plain.to_vec());
                        }
                    }
                }
                Provider::Identity => {
                    if !data.starts_with(b"k8s:enc:") {
                        return Ok(data.to_vec());
                    }
                }
            }
        }
        Err("no matching prefix found (was the value written with a key that is no longer configured?)".into())
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub const CONFIG: &str = "apiVersion: apiserver.config.k8s.io/v1\nkind: EncryptionConfiguration\nresources:\n  - resources: [secrets]\n    providers:\n      - aesgcm:\n          keys:\n            - name: key1\n              secret: dGhpcnR5LXR3by1ieXRlLWtleS1mb3ItdGVzdGluZyE=\n      - identity: {}\n";

    #[test]
    fn encrypt_decrypt_and_rotation() {
        let t = Transformer::from_yaml(CONFIG).unwrap();
        let k = b"/registry/secrets/default/s";
        let enc = t.to_storage(b"{\"a\":1}", k, "/registry/secrets/default/s").unwrap();
        assert!(enc.starts_with(b"k8s:enc:aesgcm:v1:key1:"));
        assert_eq!(t.from_storage(&enc, k, "/registry/secrets/default/s").unwrap(), b"{\"a\":1}");
        // The etcd key is authenticated: moving the value to another key fails.
        assert!(t.from_storage(&enc, b"/registry/secrets/default/other", "/registry/secrets/default/other").is_err());
        // Old plaintext secrets are still readable through identity.
        assert_eq!(t.from_storage(b"{}", k, "/registry/secrets/default/s").unwrap(), b"{}");
        // Other resources are untouched.
        assert_eq!(t.to_storage(b"{}", b"x", "/registry/configmaps/d/c").unwrap(), b"{}");
        // Rotation: new key first, old key still decrypts.
        let rotated = CONFIG.replace(
            "            - name: key1",
            "            - name: key2\n              secret: MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE=\n            - name: key1",
        );
        let t2 = Transformer::from_yaml(&rotated).unwrap();
        assert_eq!(t2.from_storage(&enc, k, "/registry/secrets/default/s").unwrap(), b"{\"a\":1}");
        assert!(t2.to_storage(b"{}", k, "/registry/secrets/default/s").unwrap().starts_with(b"k8s:enc:aesgcm:v1:key2:"));
        assert!(Transformer::from_yaml("resources:\n  - resources: [secrets]\n    providers:\n      - secretbox: {}\n").is_err());
        assert_eq!(resource_of_key("/registry/services/specs/default/x"), "services");
        assert_eq!(resource_of_key("/registry/minions/n1"), "nodes");
    }
}
