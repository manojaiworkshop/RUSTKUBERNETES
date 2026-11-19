//! Port of `plugin/pkg/auth/authenticator/token/bootstrap`: bearer tokens
//! `<id>.<secret>` backed by `kube-system/bootstrap-token-<id>` Secrets of type
//! `bootstrap.kubernetes.io/token` (`--enable-bootstrap-token-auth`).
//!
//! The user is `system:bootstrap:<id>` in `system:bootstrappers` plus the
//! Secret's `auth-extra-groups` (each `system:bootstrappers:*`).

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use k8s_apiserver::authentication::{AuthRequest, Authenticator, UserInfo};
use k8s_apiserver::storage::Storage;
use serde_json::Value;

pub const BOOTSTRAP_TOKEN_SECRET_PREFIX: &str = "bootstrap-token-";
pub const SECRET_TYPE_BOOTSTRAP_TOKEN: &str = "bootstrap.kubernetes.io/token";
pub const BOOTSTRAP_USER_PREFIX: &str = "system:bootstrap:";
pub const BOOTSTRAP_DEFAULT_GROUP: &str = "system:bootstrappers";

/// `ParseToken`: `[a-z0-9]{6}.[a-z0-9]{16}`.
pub fn parse_token(t: &str) -> Option<(&str, &str)> {
    let (id, secret) = t.split_once('.')?;
    let ok = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    (ok(id, 6) && ok(secret, 16)).then_some((id, secret))
}

/// `ValidateBootstrapGroupName`: `system:bootstrappers:[a-z0-9:-]{0,255}[a-z0-9]`.
pub fn validate_bootstrap_group_name(g: &str) -> Result<(), String> {
    let ok = g.strip_prefix("system:bootstrappers:").is_some_and(|rest| {
        !rest.is_empty()
            && rest.len() <= 256
            && rest.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b':' || b == b'-')
            && rest.bytes().last().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    });
    if ok {
        Ok(())
    } else {
        Err(format!("bootstrap group {g:?} is invalid (must match \\Asystem:bootstrappers:[a-z0-9:-]{{0,255}}[a-z0-9]\\z)"))
    }
}

/// `GetData`
fn data(secret: &Value, key: &str) -> String {
    secret
        .pointer(&format!("/data/{key}"))
        .and_then(Value::as_str)
        .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// `HasExpired`: an unparseable expiration counts as expired.
pub fn has_expired(secret: &Value, now: chrono::DateTime<chrono::Utc>) -> bool {
    let exp = data(secret, "expiration");
    if exp.is_empty() {
        return false;
    }
    match chrono::DateTime::parse_from_rfc3339(&exp) {
        Ok(t) => t.with_timezone(&chrono::Utc) <= now,
        Err(_) => true,
    }
}

/// `GetGroups`: sorted, `system:bootstrappers` included.
pub fn groups(secret: &Value) -> Result<Vec<String>, String> {
    let mut g = std::collections::BTreeSet::from([BOOTSTRAP_DEFAULT_GROUP.to_string()]);
    let extra = data(secret, "auth-extra-groups");
    if !extra.is_empty() {
        for group in extra.split(',') {
            validate_bootstrap_group_name(group)?;
            g.insert(group.to_string());
        }
    }
    Ok(g.into_iter().collect())
}

pub struct BootstrapTokenAuthenticator {
    pub storage: Arc<dyn Storage>,
}

fn token_error(secret: &Value, msg: &str) {
    let name = secret.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or("");
    tracing::debug!("Bootstrap secret kube-system/{name} matching bearer token {msg}");
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[async_trait]
impl Authenticator for BootstrapTokenAuthenticator {
    async fn authenticate(&self, req: &AuthRequest<'_>) -> Result<Option<UserInfo>, String> {
        let Some((id, token_secret)) = req.bearer_token().and_then(parse_token) else { return Ok(None) };
        let key = format!("/registry/secrets/kube-system/{BOOTSTRAP_TOKEN_SECRET_PREFIX}{id}");
        let Ok(secret) = self.storage.get(&key).await else {
            tracing::debug!("No secret of name {BOOTSTRAP_TOKEN_SECRET_PREFIX}{id} to match bootstrap bearer token");
            return Ok(None);
        };
        if secret.pointer("/metadata/deletionTimestamp").is_some_and(|d| !d.is_null()) {
            token_error(&secret, "is deleted and awaiting removal");
            return Ok(None);
        }
        if secret.get("type").and_then(Value::as_str) != Some(SECRET_TYPE_BOOTSTRAP_TOKEN) || secret.get("data").is_none() {
            token_error(&secret, &format!("has invalid type, expected {SECRET_TYPE_BOOTSTRAP_TOKEN}."));
            return Ok(None);
        }
        if !constant_time_eq(data(&secret, "token-secret").as_bytes(), token_secret.as_bytes()) {
            token_error(&secret, "has invalid value for key token-secret.");
            return Ok(None);
        }
        if data(&secret, "token-id") != id {
            token_error(&secret, "has invalid value for key token-id.");
            return Ok(None);
        }
        if has_expired(&secret, chrono::Utc::now()) {
            return Ok(None);
        }
        if data(&secret, "usage-bootstrap-authentication") != "true" {
            token_error(&secret, "not marked usage-bootstrap-authentication=true.");
            return Ok(None);
        }
        let groups = match groups(&secret) {
            Ok(g) => g,
            Err(e) => {
                token_error(&secret, &format!("has invalid value for key auth-extra-groups: {e}."));
                return Ok(None);
            }
        };
        Ok(Some(UserInfo { name: format!("{BOOTSTRAP_USER_PREFIX}{id}"), groups, ..Default::default() }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_apiserver::MemoryStore;
    use serde_json::json;

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(s)
    }

    #[tokio::test]
    async fn bootstrap_tokens() {
        let st: Arc<dyn Storage> = MemoryStore::new();
        st.create(
            "/registry/secrets/kube-system/bootstrap-token-abcdef",
            json!({"metadata": {"name": "bootstrap-token-abcdef", "namespace": "kube-system"}, "type": SECRET_TYPE_BOOTSTRAP_TOKEN,
                "data": {"token-id": b64("abcdef"), "token-secret": b64("0123456789abcdef"), "usage-bootstrap-authentication": b64("true"),
                         "auth-extra-groups": b64("system:bootstrappers:kubeadm:default-node-token")}}),
        )
        .await
        .unwrap();
        let a = BootstrapTokenAuthenticator { storage: st.clone() };
        let headers: axum::http::HeaderMap =
            [(axum::http::header::AUTHORIZATION, "Bearer abcdef.0123456789abcdef".parse().unwrap())].into_iter().collect();
        let req = AuthRequest { headers: &headers, peer_certs: None };
        let u = a.authenticate(&req).await.unwrap().unwrap();
        assert_eq!(u.name, "system:bootstrap:abcdef");
        assert_eq!(u.groups, ["system:bootstrappers", "system:bootstrappers:kubeadm:default-node-token"]);
        let bad: axum::http::HeaderMap =
            [(axum::http::header::AUTHORIZATION, "Bearer abcdef.0123456789abcdee".parse().unwrap())].into_iter().collect();
        assert!(a.authenticate(&AuthRequest { headers: &bad, peer_certs: None }).await.unwrap().is_none());
        assert!(parse_token("ABCDEF.0123456789abcdef").is_none());
        assert!(validate_bootstrap_group_name("system:bootstrappers:x:").is_err());
        assert!(has_expired(&json!({"data": {"expiration": b64("2000-01-01T00:00:00Z")}}), chrono::Utc::now()));
    }
}
