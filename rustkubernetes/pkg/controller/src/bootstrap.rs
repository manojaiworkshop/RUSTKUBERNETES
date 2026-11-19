//! Port of `pkg/controller/bootstrap`: bootstrap tokens (`kube-system` Secrets
//! of type `bootstrap.kubernetes.io/token`). Both controllers are off by
//! default; kubeadm enables them (`--controllers=*,bootstrapsigner,tokencleaner`).
//!
//! * `token-cleaner-controller`: deletes tokens past their `expiration`.
//! * `bootstrap-signer-controller`: adds `jws-kubeconfig-<token-id>` to the
//!   `kube-public/cluster-info` ConfigMap for each token with
//!   `usage-bootstrap-signing=true`: a detached HS256 JWS of the ConfigMap's
//!   `kubeconfig`, keyed with the token secret, so a joining node can trust the
//!   cluster CA it reads anonymously.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use k8s_api::core::v1::{ConfigMap, Secret};
use k8s_client_go::informers::SharedInformerFactory;
use k8s_client_go::tools::cache::Store;
use k8s_client_go::util::workqueue::WorkQueue;
use k8s_client_go::{Api, Client};

use crate::controller_utils::*;
use crate::ControllerContext;

pub const SECRET_TYPE_BOOTSTRAP_TOKEN: &str = "bootstrap.kubernetes.io/token";
pub const BOOTSTRAP_TOKEN_SECRET_PREFIX: &str = "bootstrap-token-";
pub const JWS_SIGNATURE_KEY_PREFIX: &str = "jws-kubeconfig-";
pub const CONFIG_MAP_CLUSTER_INFO: &str = "cluster-info";
pub const KUBE_CONFIG_KEY: &str = "kubeconfig";

fn data(s: &Secret, key: &str) -> String {
    s.data
        .as_ref()
        .and_then(|d| d.get(key))
        .and_then(|v| base64::engine::general_purpose::STANDARD.decode(v).ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// `GetExpiration`: (time remaining, expired). Unparseable → expired.
pub fn expiration(s: &Secret, now: chrono::DateTime<chrono::Utc>) -> (Duration, bool) {
    let exp = data(s, "expiration");
    if exp.is_empty() {
        return (Duration::ZERO, false);
    }
    match chrono::DateTime::parse_from_rfc3339(&exp) {
        Ok(t) => {
            let left = t.with_timezone(&chrono::Utc) - now;
            if left <= chrono::Duration::zero() {
                (Duration::ZERO, true)
            } else {
                (left.to_std().unwrap_or_default(), false)
            }
        }
        Err(_) => (Duration::ZERO, true),
    }
}

/// `jws.ComputeDetachedSignature`: `<header>..<signature>`.
pub fn compute_detached_signature(content: &str, token_id: &str, token_secret: &str) -> String {
    let mut key = token_secret.as_bytes().to_vec();
    if key.len() < 32 {
        key.resize(32, 0);
    }
    let b64 = |d: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(d);
    let header = b64(format!(r#"{{"alg":"HS256","kid":"{token_id}"}}"#).as_bytes());
    let signing_input = format!("{header}.{}", b64(content.as_bytes()));
    let tag = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &key), signing_input.as_bytes());
    format!("{header}..{}", b64(tag.as_ref()))
}

fn is_bootstrap_token(s: &Secret) -> bool {
    s.metadata.namespace.as_deref() == Some("kube-system") && s.type_.as_deref() == Some(SECRET_TYPE_BOOTSTRAP_TOKEN)
}

/// `TokenCleaner`
pub struct TokenCleaner {
    client: Client,
    secrets: Store<Secret>,
    informers: Arc<SharedInformerFactory>,
    queue: Queue,
}

impl TokenCleaner {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let inf = ctx.informers.informer::<Secret>();
        let c =
            Arc::new(Self { client: ctx.client.clone(), secrets: inf.store(), informers: ctx.informers.clone(), queue: WorkQueue::new() });
        let me = c.clone();
        on_events(inf.subscribe(), move |ev| {
            if is_bootstrap_token(ev.object()) {
                me.queue.add(key(ev.object().as_ref()));
            }
        });
        c
    }

    pub async fn run(self: Arc<Self>, workers: usize) {
        self.informers.wait_for_cache_sync().await;
        let me = self.clone();
        run_workers("token-cleaner-controller", self.queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.eval_secret(&k).await }
        })
        .await;
    }

    /// `evalSecret`
    async fn eval_secret(&self, k: &str) -> SyncResult {
        let Some(s) = self.secrets.get(k) else { return Ok(None) };
        let (ttl, expired) = expiration(&s, chrono::Utc::now());
        if expired {
            tracing::info!(secret = k, "Deleting expired secret");
            let api: Api<Secret> = Api::namespaced(self.client.clone(), "kube-system");
            match api.delete(s.metadata.name.as_deref().unwrap_or_default()).await {
                Ok(_) => {}
                Err(e) if e.is_not_found() || e.is_conflict() => {}
                Err(e) => tracing::debug!("Error deleting secret: {e}"),
            }
            return Ok(None);
        }
        Ok((ttl > Duration::ZERO).then_some(ttl))
    }
}

/// `Signer` (bootstrapsigner)
pub struct Signer {
    client: Client,
    secrets: Store<Secret>,
    config_maps: Store<ConfigMap>,
    informers: Arc<SharedInformerFactory>,
    queue: Queue,
}

impl Signer {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let s_inf = ctx.informers.informer::<Secret>();
        let cm_inf = ctx.informers.informer::<ConfigMap>();
        let c = Arc::new(Self {
            client: ctx.client.clone(),
            secrets: s_inf.store(),
            config_maps: cm_inf.store(),
            informers: ctx.informers.clone(),
            queue: WorkQueue::new(),
        });
        let me = c.clone();
        on_events(cm_inf.subscribe(), move |ev| {
            let m = &ev.object().metadata;
            if m.namespace.as_deref() == Some("kube-public") && m.name.as_deref() == Some(CONFIG_MAP_CLUSTER_INFO) {
                me.queue.add("signer".into());
            }
        });
        let me = c.clone();
        on_events(s_inf.subscribe(), move |ev| {
            if is_bootstrap_token(ev.object()) {
                me.queue.add("signer".into());
            }
        });
        c
    }

    pub async fn run(self: Arc<Self>, _workers: usize) {
        self.informers.wait_for_cache_sync().await;
        self.queue.add("signer".into());
        let me = self.clone();
        run_workers("bootstrap-signer-controller", self.queue.clone(), 1, move |_| {
            let me = me.clone();
            async move { me.sign_config_map().await }
        })
        .await;
    }

    /// `getTokens` (`validateSecretForSigning`)
    fn tokens(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for s in self.secrets.list_namespace("kube-system") {
            if !is_bootstrap_token(&s) {
                continue;
            }
            let Some(name_id) = s.metadata.name.as_deref().and_then(|n| n.strip_prefix(BOOTSTRAP_TOKEN_SECRET_PREFIX)) else { continue };
            let id = data(&s, "token-id");
            let secret = data(&s, "token-secret");
            if id.is_empty() || id != name_id || secret.is_empty() {
                continue;
            }
            if expiration(&s, chrono::Utc::now()).1 || data(&s, "usage-bootstrap-signing") != "true" {
                continue;
            }
            out.entry(id).or_insert(secret);
        }
        out
    }

    /// `signConfigMap`
    async fn sign_config_map(&self) -> SyncResult {
        let Some(orig) = self.config_maps.get_ns("kube-public", CONFIG_MAP_CLUSTER_INFO) else { return Ok(None) };
        let mut cm = (*orig).clone();
        let data = cm.data.get_or_insert_with(Default::default);
        let Some(content) = data.get(KUBE_CONFIG_KEY).cloned() else {
            tracing::debug!("No {KUBE_CONFIG_KEY} key in ConfigMap kube-public/{CONFIG_MAP_CLUSTER_INFO}");
            return Ok(None);
        };
        let mut sigs: BTreeMap<String, String> = BTreeMap::new();
        data.retain(|k, v| match k.strip_prefix(JWS_SIGNATURE_KEY_PREFIX) {
            Some(id) => {
                sigs.insert(id.to_string(), v.clone());
                false
            }
            None => true,
        });
        let mut need_update = false;
        for (id, secret) in self.tokens() {
            let sig = compute_detached_signature(&content, &id, &secret);
            if sigs.remove(&id).as_deref() != Some(sig.as_str()) {
                need_update = true;
            }
            data.insert(format!("{JWS_SIGNATURE_KEY_PREFIX}{id}"), sig);
        }
        if !sigs.is_empty() {
            need_update = true;
        }
        if need_update {
            let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), "kube-public");
            api.replace(&cm).await?;
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detached_jws() {
        // Same value as `jws.ComputeDetachedSignature("foo", "abcdef", "abcdef0123456789")` in Go.
        let sig = compute_detached_signature("foo", "abcdef", "abcdef0123456789");
        let (header, rest) = sig.split_once("..").unwrap();
        assert_eq!(
            String::from_utf8(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(header).unwrap()).unwrap(),
            r#"{"alg":"HS256","kid":"abcdef"}"#
        );
        assert_eq!(rest.len(), 43);
        // Cross-checked with Python's hmac/hashlib (go-jose produces the same compact form).
        assert_eq!(sig, "eyJhbGciOiJIUzI1NiIsImtpZCI6ImFiY2RlZiJ9..f_akjwCRL9bIa1GF6CvBk7CaS19Tn3RN-uWQ-FFayFc");
        assert_ne!(sig, compute_detached_signature("bar", "abcdef", "abcdef0123456789"));
    }
}
