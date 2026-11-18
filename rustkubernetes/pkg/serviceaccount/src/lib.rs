//! Port of `k8s.io/kubernetes/pkg/serviceaccount`: ServiceAccount tokens.
//!
//! Tokens are ES256 JWTs (`--service-account-signing-key-file`, ECDSA P-256)
//! with upstream's claim layout:
//!
//! ```json
//! {"iss": "https://kubernetes.default.svc.cluster.local", "aud": ["https://kubernetes.default.svc.cluster.local"],
//!  "sub": "system:serviceaccount:<ns>:<name>", "exp": ..., "iat": ..., "nbf": ..., "jti": "<uuid>",
//!  "kubernetes.io": {"namespace": "<ns>", "serviceaccount": {"name": "...", "uid": "..."},
//!                    "pod": {"name": "...", "uid": "..."}, "warnafter": ...}}
//! ```
//!
//! [`TokenAuthenticator`] verifies signature, issuer, audience and expiry, then
//! checks that the ServiceAccount (and the bound Pod, if any) still exist with the
//! same UID — deleting the pod revokes its tokens.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use k8s_apiserver::authentication::{AuthRequest, Authenticator, UserInfo};
use k8s_apiserver::Storage;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::SubjectPublicKeyInfoDer;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const USER_PREFIX: &str = "system:serviceaccount:";
pub const DEFAULT_ISSUER: &str = "https://kubernetes.default.svc.cluster.local";

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ref {
    pub name: String,
    pub uid: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct KubernetesClaims {
    pub namespace: String,
    pub serviceaccount: Ref,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pod: Option<Ref>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<Ref>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<Ref>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnafter: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claims {
    pub iss: String,
    pub sub: String,
    pub aud: Vec<String>,
    pub exp: i64,
    pub iat: i64,
    pub nbf: i64,
    pub jti: String,
    #[serde(rename = "kubernetes.io")]
    pub kubernetes: KubernetesClaims,
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// `kid`: base64url(SHA-256(DER public key)), like upstream's `keyIDFromPublicKey`.
fn key_id(spki_der: &[u8]) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, spki_der);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(d.as_ref())
}

/// Public verification keys (`--service-account-key-file`, may hold several).
#[derive(Clone)]
pub struct PublicKeys {
    keys: Vec<(String, DecodingKey, Vec<u8>)>,
}

impl PublicKeys {
    pub fn from_pem(pem: &[u8]) -> Result<Self, String> {
        let mut keys = vec![];
        for der in SubjectPublicKeyInfoDer::pem_slice_iter(pem) {
            let der = der.map_err(|e| format!("parsing public key: {e}"))?;
            let pem_one = format!(
                "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
                base64::engine::general_purpose::STANDARD.encode(der.as_ref())
            );
            let k = DecodingKey::from_ec_pem(pem_one.as_bytes()).map_err(|e| format!("unsupported public key (want ECDSA P-256): {e}"))?;
            keys.push((key_id(der.as_ref()), k, der.as_ref().to_vec()));
        }
        if keys.is_empty() {
            return Err("no public keys found".into());
        }
        Ok(Self { keys })
    }

    /// The JWKS document served at `/openid/v1/jwks`.
    pub fn jwks(&self) -> Value {
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        let keys: Vec<Value> = self
            .keys
            .iter()
            .filter(|(_, _, der)| der.len() >= 65)
            .map(|(kid, _, der)| {
                // A P-256 SPKI ends with the uncompressed point 04 || X(32) || Y(32).
                let point = &der[der.len() - 65..];
                json!({"use": "sig", "kty": "EC", "kid": kid, "crv": "P-256", "alg": "ES256", "x": b64(&point[1..33]), "y": b64(&point[33..])})
            })
            .collect();
        json!({"keys": keys})
    }
}

/// Issues tokens (`TokenGenerator`).
pub struct TokenGenerator {
    pub issuer: String,
    key: EncodingKey,
    kid: String,
}

impl TokenGenerator {
    /// `signing_key_pem`: PKCS#8 ECDSA P-256 private key; `public_pem`: its public key (for `kid`).
    pub fn new(issuer: &str, signing_key_pem: &[u8], public_pem: &[u8]) -> Result<Self, String> {
        let key =
            EncodingKey::from_ec_pem(signing_key_pem).map_err(|e| format!("unsupported signing key (want ECDSA P-256 PKCS#8): {e}"))?;
        let der = SubjectPublicKeyInfoDer::from_pem_slice(public_pem).map_err(|e| format!("parsing public key: {e}"))?;
        Ok(Self { issuer: issuer.to_string(), key, kid: key_id(der.as_ref()) })
    }

    /// Signs a token for `namespace/sa` valid for `ttl` seconds.
    pub fn generate(&self, audiences: &[String], ttl: i64, kc: KubernetesClaims) -> Result<(String, i64), String> {
        let iat = now();
        let exp = iat + ttl;
        let claims = Claims {
            iss: self.issuer.clone(),
            sub: format!("{USER_PREFIX}{}:{}", kc.namespace, kc.serviceaccount.name),
            aud: audiences.to_vec(),
            exp,
            iat,
            nbf: iat,
            jti: uuid::Uuid::new_v4().to_string(),
            kubernetes: kc,
        };
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(self.kid.clone());
        let t = jsonwebtoken::encode(&header, &claims, &self.key).map_err(|e| format!("signing token: {e}"))?;
        Ok((t, exp))
    }
}

/// Verifies ServiceAccount tokens (`JWTTokenAuthenticator`).
pub struct TokenAuthenticator {
    pub issuers: Vec<String>,
    pub audiences: Vec<String>,
    pub keys: PublicKeys,
    pub storage: Arc<dyn Storage>,
}

impl TokenAuthenticator {
    /// Validates a token; `audiences` overrides the API audiences (TokenReview `spec.audiences`).
    pub async fn validate(&self, token: &str, audiences: Option<&[String]>) -> Result<(UserInfo, Vec<String>), String> {
        let header = jsonwebtoken::decode_header(token).map_err(|e| format!("invalid token: {e}"))?;
        let mut v = Validation::new(Algorithm::ES256);
        v.set_issuer(&self.issuers);
        let auds = audiences.unwrap_or(&self.audiences);
        v.set_audience(auds);
        v.leeway = 60;
        let mut last = String::from("no matching key");
        let mut claims = None;
        for (kid, k, _) in &self.keys.keys {
            if header.kid.as_ref().is_some_and(|h| h != kid) {
                continue;
            }
            match jsonwebtoken::decode::<Claims>(token, k, &v) {
                Ok(d) => {
                    claims = Some(d.claims);
                    break;
                }
                Err(e) => last = e.to_string(),
            }
        }
        let c = claims.ok_or_else(|| format!("invalid service account token: {last}"))?;
        let kc = &c.kubernetes;
        let expected_sub = format!("{USER_PREFIX}{}:{}", kc.namespace, kc.serviceaccount.name);
        if c.sub != expected_sub {
            return Err("invalid service account token: sub does not match claims".into());
        }
        // The ServiceAccount (and bound pod) must still exist with the same UID.
        let sa = self
            .storage
            .get(&format!("/registry/serviceaccounts/{}/{}", kc.namespace, kc.serviceaccount.name))
            .await
            .map_err(|_| format!("service account {}/{} has been deleted", kc.namespace, kc.serviceaccount.name))?;
        if sa["metadata"]["uid"].as_str() != Some(kc.serviceaccount.uid.as_str()) {
            return Err(format!("service account UID ({}) does not match claim", sa["metadata"]["uid"].as_str().unwrap_or("")));
        }
        let mut extra = BTreeMap::new();
        if let Some(p) = &kc.pod {
            let pod = self
                .storage
                .get(&format!("/registry/pods/{}/{}", kc.namespace, p.name))
                .await
                .map_err(|_| format!("pod {}/{} has been deleted", kc.namespace, p.name))?;
            if pod["metadata"]["uid"].as_str() != Some(p.uid.as_str()) {
                return Err(format!("pod {}/{} UID does not match claim", kc.namespace, p.name));
            }
            extra.insert("authentication.kubernetes.io/pod-name".to_string(), vec![p.name.clone()]);
            extra.insert("authentication.kubernetes.io/pod-uid".to_string(), vec![p.uid.clone()]);
        }
        extra.insert("authentication.kubernetes.io/credential-id".to_string(), vec![format!("JTI={}", c.jti)]);
        let user = UserInfo {
            name: expected_sub,
            uid: kc.serviceaccount.uid.clone(),
            groups: vec!["system:serviceaccounts".into(), format!("system:serviceaccounts:{}", kc.namespace)],
            extra,
        };
        let matched: Vec<String> = c.aud.iter().filter(|a| auds.contains(a)).cloned().collect();
        Ok((user, matched))
    }
}

/// Only JWTs from one of our issuers are this authenticator's business.
fn looks_like_ours(token: &str, issuers: &[String]) -> bool {
    let Some(payload) = token.split('.').nth(1) else { return false };
    let Ok(raw) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')) else { return false };
    let Ok(v) = serde_json::from_slice::<Value>(&raw) else { return false };
    v["iss"].as_str().is_some_and(|i| issuers.iter().any(|x| x == i))
}

#[async_trait]
impl Authenticator for TokenAuthenticator {
    async fn authenticate(&self, req: &AuthRequest<'_>) -> Result<Option<UserInfo>, String> {
        let Some(t) = req.bearer_token() else { return Ok(None) };
        if !looks_like_ours(t, &self.issuers) {
            return Ok(None);
        }
        self.validate(t, None).await.map(|(u, _)| Some(u))
    }
}

/// `/.well-known/openid-configuration`.
pub fn openid_configuration(issuer: &str) -> Value {
    json!({
        "issuer": issuer,
        "jwks_uri": format!("{}/openid/v1/jwks", issuer.trim_end_matches('/')),
        "response_types_supported": ["id_token"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["ES256"],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_apiserver::MemoryStore;

    #[tokio::test]
    async fn issue_and_validate() {
        let kp = rcgen::KeyPair::generate().unwrap();
        let (priv_pem, pub_pem) = (kp.serialize_pem(), kp.public_key_pem());
        let gen = TokenGenerator::new(DEFAULT_ISSUER, priv_pem.as_bytes(), pub_pem.as_bytes()).unwrap();
        let store = MemoryStore::new();
        let st: Arc<dyn Storage> = store.clone();
        st.create("/registry/serviceaccounts/dev/ci", json!({"metadata": {"name": "ci", "namespace": "dev", "uid": "sa-1"}}))
            .await
            .unwrap();
        st.create("/registry/pods/dev/p", json!({"metadata": {"name": "p", "namespace": "dev", "uid": "pod-1"}})).await.unwrap();
        let auth = TokenAuthenticator {
            issuers: vec![DEFAULT_ISSUER.into()],
            audiences: vec![DEFAULT_ISSUER.into()],
            keys: PublicKeys::from_pem(pub_pem.as_bytes()).unwrap(),
            storage: st.clone(),
        };
        let kc = KubernetesClaims {
            namespace: "dev".into(),
            serviceaccount: Ref { name: "ci".into(), uid: "sa-1".into() },
            pod: Some(Ref { name: "p".into(), uid: "pod-1".into() }),
            ..Default::default()
        };
        let (tok, _) = gen.generate(&[DEFAULT_ISSUER.into()], 3600, kc.clone()).unwrap();
        let (u, _) = auth.validate(&tok, None).await.unwrap();
        assert_eq!(u.name, "system:serviceaccount:dev:ci");
        assert_eq!(u.groups, ["system:serviceaccounts", "system:serviceaccounts:dev"]);
        assert_eq!(u.extra["authentication.kubernetes.io/pod-name"], ["p"]);
        // Wrong audience.
        let (other, _) = gen.generate(&["vault".into()], 3600, kc.clone()).unwrap();
        assert!(auth.validate(&other, None).await.is_err());
        assert!(auth.validate(&other, Some(&["vault".to_string()])).await.is_ok());
        // Deleting the pod revokes the token.
        st.delete("/registry/pods/dev/p", None).await.unwrap();
        assert!(auth.validate(&tok, None).await.unwrap_err().contains("deleted"));
        // Tampered token.
        let mut parts: Vec<&str> = tok.split('.').collect();
        let fake = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"iss":"x"}"#);
        parts[1] = &fake;
        assert!(auth.validate(&parts.join("."), None).await.is_err());
        let jwks = PublicKeys::from_pem(pub_pem.as_bytes()).unwrap().jwks();
        assert_eq!(jwks["keys"][0]["crv"], "P-256");
    }
}
