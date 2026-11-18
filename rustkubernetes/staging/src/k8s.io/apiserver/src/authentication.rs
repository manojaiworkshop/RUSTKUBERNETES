//! Port of `k8s.io/apiserver/pkg/authentication`: who is making a request.
//!
//! Authenticators are tried in order (`union.New`); the first one that
//! recognises the credentials wins. Invalid credentials fail the request with
//! 401 even when anonymous access is enabled. Authenticated users always get the
//! `system:authenticated` group.
//!
//! | Authenticator | Credentials | Go equivalent |
//! |---------------|-------------|---------------|
//! | [`X509Authenticator`] | verified TLS client certificate (CN = user, O = groups) | `request/x509` |
//! | [`TokenFileAuthenticator`] | `Authorization: Bearer` from `--token-auth-file` | `token/tokenfile` |
//! | ServiceAccount JWTs | provided by `kube-serviceaccount` | `pkg/serviceaccount` |
//! | anonymous | none | `request/anonymous` |

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use axum::http::HeaderMap;
use rustls_pki_types::CertificateDer;
use serde_json::{json, Value};

pub const USER_ANONYMOUS: &str = "system:anonymous";
pub const GROUP_UNAUTHENTICATED: &str = "system:unauthenticated";
pub const GROUP_AUTHENTICATED: &str = "system:authenticated";
pub const GROUP_MASTERS: &str = "system:masters";

/// `user.Info`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserInfo {
    pub name: String,
    pub uid: String,
    pub groups: Vec<String>,
    pub extra: BTreeMap<String, Vec<String>>,
}

impl UserInfo {
    pub fn new(name: impl Into<String>, groups: &[&str]) -> Self {
        Self { name: name.into(), groups: groups.iter().map(|g| g.to_string()).collect(), ..Default::default() }
    }

    pub fn anonymous() -> Self {
        Self::new(USER_ANONYMOUS, &[GROUP_UNAUTHENTICATED])
    }

    /// Identity used by the insecure (no TLS) development server: full access.
    pub fn insecure_admin() -> Self {
        Self::new("system:admin", &[GROUP_MASTERS, GROUP_AUTHENTICATED])
    }

    pub fn in_group(&self, g: &str) -> bool {
        self.groups.iter().any(|x| x == g)
    }

    /// `UserInfo` as used in TokenReview / SelfSubjectReview status.
    pub fn to_json(&self) -> Value {
        let mut v = json!({"username": self.name, "groups": self.groups});
        if !self.uid.is_empty() {
            v["uid"] = json!(self.uid);
        }
        if !self.extra.is_empty() {
            v["extra"] = json!(self.extra);
        }
        v
    }

    fn with_authenticated_group(mut self) -> Self {
        if self.name != USER_ANONYMOUS && !self.in_group(GROUP_AUTHENTICATED) {
            self.groups.push(GROUP_AUTHENTICATED.into());
        }
        self
    }
}

/// What an authenticator can look at.
pub struct AuthRequest<'a> {
    pub headers: &'a HeaderMap,
    /// Client certificate chain already verified against `--client-ca-file` by TLS.
    pub peer_certs: Option<&'a [CertificateDer<'static>]>,
}

impl AuthRequest<'_> {
    pub fn bearer_token(&self) -> Option<&str> {
        let h = self.headers.get("authorization")?.to_str().ok()?;
        let (scheme, token) = h.split_once(' ')?;
        (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()).then(|| token.trim())
    }
}

#[async_trait]
pub trait Authenticator: Send + Sync {
    /// `Ok(None)`: no credentials of this kind; `Err`: credentials present but invalid.
    async fn authenticate(&self, req: &AuthRequest<'_>) -> Result<Option<UserInfo>, String>;
}

/// `union` + `anonymous` + group adder.
pub struct AuthenticatorChain {
    pub authenticators: Vec<Arc<dyn Authenticator>>,
    pub anonymous: bool,
}

impl AuthenticatorChain {
    pub async fn authenticate(&self, req: &AuthRequest<'_>) -> Result<UserInfo, String> {
        let mut errors = vec![];
        for a in &self.authenticators {
            match a.authenticate(req).await {
                Ok(Some(u)) => return Ok(u.with_authenticated_group()),
                Ok(None) => {}
                Err(e) => errors.push(e),
            }
        }
        if !errors.is_empty() {
            return Err(errors.join("; "));
        }
        if self.anonymous {
            Ok(UserInfo::anonymous())
        } else {
            Err("no credentials provided".into())
        }
    }
}

/// CommonName and Organizations of a DER certificate.
pub fn subject_of(der: &[u8]) -> Result<(String, Vec<String>), String> {
    use x509_parser::prelude::*;
    let (_, cert) = X509Certificate::from_der(der).map_err(|e| format!("parsing client certificate: {e}"))?;
    let cn = cert.subject().iter_common_name().next().and_then(|a| a.as_str().ok()).unwrap_or("").to_string();
    let orgs = cert.subject().iter_organization().filter_map(|a| a.as_str().ok().map(String::from)).collect();
    Ok((cn, orgs))
}

/// `request/x509.CommonNameUserConversion`.
pub struct X509Authenticator;

#[async_trait]
impl Authenticator for X509Authenticator {
    async fn authenticate(&self, req: &AuthRequest<'_>) -> Result<Option<UserInfo>, String> {
        let Some(leaf) = req.peer_certs.and_then(|c| c.first()) else { return Ok(None) };
        let (cn, orgs) = subject_of(leaf)?;
        if cn.is_empty() {
            return Err("client certificate has no common name".into());
        }
        Ok(Some(UserInfo { name: cn, groups: orgs, ..Default::default() }))
    }
}

/// `--token-auth-file`: CSV lines `token,user,uid,"group1,group2"`.
pub struct TokenFileAuthenticator {
    tokens: HashMap<String, UserInfo>,
}

impl TokenFileAuthenticator {
    pub fn parse(csv: &str) -> Result<Self, String> {
        let mut tokens = HashMap::new();
        for (n, line) in csv.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields = split_csv(line);
            if fields.len() < 3 {
                return Err(format!("token file line {}: want token,user,uid[,\"groups\"]", n + 1));
            }
            let groups =
                fields.get(3).map(|g| g.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
            tokens.insert(fields[0].clone(), UserInfo { name: fields[1].clone(), uid: fields[2].clone(), groups, ..Default::default() });
        }
        Ok(Self { tokens })
    }
}

fn split_csv(line: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().map(|s| s.trim().to_string()).collect()
}

#[async_trait]
impl Authenticator for TokenFileAuthenticator {
    async fn authenticate(&self, req: &AuthRequest<'_>) -> Result<Option<UserInfo>, String> {
        let Some(t) = req.bearer_token() else { return Ok(None) };
        // A JWT-shaped token is left to the ServiceAccount authenticator.
        match self.tokens.get(t) {
            Some(u) => Ok(Some(u.clone())),
            None => Ok(None),
        }
    }
}

/// Fails requests whose bearer token nobody recognised (instead of falling back to anonymous).
pub struct RejectUnknownBearer;

#[async_trait]
impl Authenticator for RejectUnknownBearer {
    async fn authenticate(&self, req: &AuthRequest<'_>) -> Result<Option<UserInfo>, String> {
        match req.bearer_token() {
            Some(_) => Err("invalid bearer token".into()),
            None => Ok(None),
        }
    }
}

tokio::task_local! {
    /// The authenticated (possibly impersonated) user of the request being served.
    pub static CURRENT_USER: UserInfo;
}

/// The user of the current request (`request.UserFrom(ctx)`).
pub fn current_user() -> Option<UserInfo> {
    CURRENT_USER.try_with(Clone::clone).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn token_file_and_anonymous() {
        let a = TokenFileAuthenticator::parse("# comment\nsecret123,jane,u1,\"dev,ops\"\nother,bob,u2\n").unwrap();
        let chain = AuthenticatorChain { authenticators: vec![Arc::new(a), Arc::new(RejectUnknownBearer)], anonymous: true };
        let mut h = HeaderMap::new();
        let u = chain.authenticate(&AuthRequest { headers: &h, peer_certs: None }).await.unwrap();
        assert_eq!(u, UserInfo::anonymous());
        h.insert("authorization", "Bearer secret123".parse().unwrap());
        let u = chain.authenticate(&AuthRequest { headers: &h, peer_certs: None }).await.unwrap();
        assert_eq!(u.name, "jane");
        assert_eq!(u.groups, ["dev", "ops", GROUP_AUTHENTICATED]);
        h.insert("authorization", "Bearer nope".parse().unwrap());
        assert!(chain.authenticate(&AuthRequest { headers: &h, peer_certs: None }).await.is_err());
    }
}
