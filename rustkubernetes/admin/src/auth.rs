//! Sign-in and permissions.
//!
//! * The super admin comes from `RUSTKUBEADMIN_USER` / `RUSTKUBEADMIN_PASSWORD` and
//!   may do everything; other users are created by an admin and stored with
//!   PBKDF2-HMAC-SHA256 password hashes.
//! * A successful sign-in sets an HttpOnly, SameSite=Strict session cookie
//!   (`rkadmin_session`); sessions live in memory for 12 hours.
//! * Console permissions (`users.manage`, `machines.manage`, `clusters.manage`,
//!   `audit.view`) gate console actions; what a user may do *inside* a cluster is
//!   decided by Kubernetes RBAC, because the console impersonates the user.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use ring::rand::SecureRandom;
use serde_json::{json, Value};

use crate::state::Grant;

pub const COOKIE: &str = "rkadmin_session";
const ITERATIONS: u32 = 210_000;
const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);
/// Identity used towards clusters for console user `<name>`.
pub const KUBE_USER_PREFIX: &str = "rustkubeadmin:";
pub const KUBE_GROUP: &str = "rustkubeadmin:users";

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD_NO_PAD
}

pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    ring::rand::SystemRandom::new().fill(&mut b).expect("system random");
    b
}

pub fn random_token() -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes(32))
}

pub fn hash_password(password: &str) -> String {
    let salt = random_bytes(16);
    let mut out = [0u8; 32];
    ring::pbkdf2::derive(ring::pbkdf2::PBKDF2_HMAC_SHA256, NonZeroU32::new(ITERATIONS).unwrap(), &salt, password.as_bytes(), &mut out);
    format!("pbkdf2-sha256${ITERATIONS}${}${}", b64().encode(salt), b64().encode(out))
}

pub fn verify_password(password: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    let [scheme, iters, salt, hash] = parts.as_slice() else { return false };
    if *scheme != "pbkdf2-sha256" {
        return false;
    }
    let (Ok(iters), Ok(salt), Ok(hash)) = (iters.parse::<u32>(), b64().decode(salt), b64().decode(hash)) else { return false };
    let Some(iters) = NonZeroU32::new(iters) else { return false };
    ring::pbkdf2::verify(ring::pbkdf2::PBKDF2_HMAC_SHA256, iters, &salt, password.as_bytes(), &hash).is_ok()
}

/// Constant-time string comparison.
pub fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Who is signed in.
#[derive(Clone, Debug)]
pub struct Principal {
    pub username: String,
    pub superadmin: bool,
    pub permissions: Vec<String>,
    pub grants: Vec<Grant>,
}

impl Principal {
    pub fn can(&self, perm: &str) -> bool {
        self.superadmin || self.permissions.iter().any(|p| p == perm)
    }

    /// The Kubernetes identity this user acts as (`None`: the super admin uses the cluster admin credentials).
    pub fn kube_identity(&self) -> Option<(String, Vec<String>)> {
        (!self.superadmin).then(|| (format!("{KUBE_USER_PREFIX}{}", self.username), vec![KUBE_GROUP.to_string()]))
    }

    /// Namespaces of `cluster` this user may see (`None` = all; empty = no access).
    pub fn visible_namespaces(&self, cluster: &str) -> Option<Vec<String>> {
        if self.superadmin {
            return None;
        }
        let gs: Vec<&Grant> = self.grants.iter().filter(|g| g.cluster == cluster).collect();
        if gs.iter().any(|g| g.namespace.is_empty()) {
            return None;
        }
        Some(gs.iter().map(|g| g.namespace.clone()).collect())
    }

    pub fn has_cluster(&self, cluster: &str) -> bool {
        self.superadmin || self.grants.iter().any(|g| g.cluster == cluster)
    }

    pub fn to_json(&self) -> Value {
        json!({"username": self.username, "superadmin": self.superadmin, "permissions": self.permissions, "grants": self.grants})
    }
}

pub struct Sessions {
    map: Mutex<HashMap<String, (String, Instant)>>,
}

impl Default for Sessions {
    fn default() -> Self {
        Self { map: Mutex::new(HashMap::new()) }
    }
}

impl Sessions {
    pub fn create(&self, username: &str) -> String {
        let t = random_token();
        let mut m = self.map.lock().unwrap();
        m.retain(|_, (_, at)| at.elapsed() < SESSION_TTL);
        m.insert(t.clone(), (username.to_string(), Instant::now()));
        t
    }

    pub fn user(&self, token: &str) -> Option<String> {
        let m = self.map.lock().unwrap();
        m.get(token).filter(|(_, at)| at.elapsed() < SESSION_TTL).map(|(u, _)| u.clone())
    }

    pub fn remove(&self, token: &str) {
        self.map.lock().unwrap().remove(token);
    }

    /// Signs a user out everywhere (deleted or disabled user, password change).
    pub fn remove_user(&self, username: &str) {
        self.map.lock().unwrap().retain(|_, (u, _)| u != username);
    }
}

pub fn cookie_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all("cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hashing() {
        let h = hash_password("s3cret!");
        assert!(h.starts_with("pbkdf2-sha256$210000$"));
        assert!(verify_password("s3cret!", &h));
        assert!(!verify_password("wrong", &h));
        assert_ne!(h, hash_password("s3cret!"), "salted");
    }

    #[test]
    fn visibility() {
        let p = Principal {
            username: "jane".into(),
            superadmin: false,
            permissions: vec![],
            grants: vec![Grant { cluster: "prod".into(), namespace: "web".into(), role: "edit".into() }],
        };
        assert_eq!(p.visible_namespaces("prod"), Some(vec!["web".to_string()]));
        assert_eq!(p.visible_namespaces("dev"), Some(vec![]));
        assert!(!p.can("users.manage"));
        assert_eq!(p.kube_identity().unwrap().0, "rustkubeadmin:jane");
    }
}
