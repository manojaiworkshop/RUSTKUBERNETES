//! Port of `client-go/tools/clientcmd`: kubeconfig loading (subset).
//!
//! Supports `current-context`, clusters (`server`, `certificate-authority[-data]`,
//! `insecure-skip-tls-verify`) and users (`client-certificate[-data]`,
//! `client-key[-data]`, `token`, `tokenFile`). Relative file paths are resolved
//! against the kubeconfig's directory, like client-go.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::rest::{Config, Error};

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Kubeconfig {
    #[serde(default)]
    pub clusters: Vec<NamedCluster>,
    #[serde(default)]
    pub contexts: Vec<NamedContext>,
    #[serde(default)]
    pub users: Vec<NamedUser>,
    #[serde(default)]
    pub current_context: String,
}

#[derive(Debug, Deserialize)]
pub struct NamedCluster {
    pub name: String,
    pub cluster: Cluster,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Cluster {
    pub server: String,
    pub certificate_authority: Option<String>,
    pub certificate_authority_data: Option<String>,
    #[serde(default)]
    pub insecure_skip_tls_verify: bool,
}

#[derive(Debug, Deserialize)]
pub struct NamedContext {
    pub name: String,
    pub context: Context,
}

#[derive(Debug, Deserialize)]
pub struct Context {
    pub cluster: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub namespace: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct NamedUser {
    pub name: String,
    #[serde(default)]
    pub user: User,
}

#[derive(Debug, Default, Deserialize)]
pub struct User {
    pub token: Option<String>,
    #[serde(rename = "tokenFile")]
    pub token_file: Option<String>,
    #[serde(rename = "client-certificate")]
    pub client_certificate: Option<String>,
    #[serde(rename = "client-certificate-data")]
    pub client_certificate_data: Option<String>,
    #[serde(rename = "client-key")]
    pub client_key: Option<String>,
    #[serde(rename = "client-key-data")]
    pub client_key_data: Option<String>,
}

/// Inline base64 data or the contents of a file (relative to the kubeconfig).
fn data_or_file(data: &Option<String>, file: &Option<String>, base: &Path) -> Result<Option<Vec<u8>>, Error> {
    use base64::Engine;
    if let Some(d) = data {
        return base64::engine::general_purpose::STANDARD
            .decode(d.trim())
            .map(Some)
            .map_err(|e| Error::Config(format!("invalid base64 data: {e}")));
    }
    if let Some(f) = file {
        let p = base.join(f);
        return std::fs::read(&p).map(Some).map_err(|e| Error::Config(format!("reading {}: {e}", p.display())));
    }
    Ok(None)
}

/// First path in `$KUBECONFIG`, else `~/.kube/config` if it exists.
pub fn default_kubeconfig_path() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("KUBECONFIG") {
        if let Some(first) = std::env::split_paths(&v).find(|p| !p.as_os_str().is_empty()) {
            return Some(first);
        }
    }
    let home = std::env::var_os("HOME")?;
    let p = PathBuf::from(home).join(".kube").join("config");
    p.exists().then_some(p)
}

pub fn load_kubeconfig(path: &Path) -> Result<Kubeconfig, Error> {
    let raw = std::fs::read_to_string(path).map_err(|e| Error::Config(format!("reading {}: {e}", path.display())))?;
    serde_yaml::from_str(&raw).map_err(|e| Error::Config(format!("parsing {}: {e}", path.display())))
}

/// Builds a [`Config`] from a kubeconfig file, using `context` or the current context.
pub fn load_config(path: &Path, context: Option<&str>) -> Result<Config, Error> {
    let kc = load_kubeconfig(path)?;
    let ctx_name = context.unwrap_or(&kc.current_context);
    let ctx = kc
        .contexts
        .iter()
        .find(|c| c.name == ctx_name)
        .ok_or_else(|| Error::Config(format!("context {ctx_name:?} not found in {}", path.display())))?;
    let cluster = kc
        .clusters
        .iter()
        .find(|c| c.name == ctx.context.cluster)
        .ok_or_else(|| Error::Config(format!("cluster {:?} not found", ctx.context.cluster)))?;
    let base = path.parent().unwrap_or(Path::new("."));
    let mut cfg = Config::new(&cluster.cluster.server);
    let c = &cluster.cluster;
    cfg.tls.ca_data = data_or_file(&c.certificate_authority_data, &c.certificate_authority, base)?;
    cfg.tls.insecure = c.insecure_skip_tls_verify;
    if let Some(u) = kc.users.iter().find(|u| u.name == ctx.context.user) {
        let u = &u.user;
        cfg.bearer_token = u.token.clone();
        cfg.bearer_token_file = u.token_file.as_ref().map(|f| base.join(f));
        cfg.tls.cert_data = data_or_file(&u.client_certificate_data, &u.client_certificate, base)?;
        cfg.tls.key_data = data_or_file(&u.client_key_data, &u.client_key, base)?;
    }
    Ok(cfg)
}

/// The namespace set on the current context (`default` if none).
pub fn current_namespace(path: &Path) -> String {
    load_kubeconfig(path)
        .ok()
        .and_then(|kc| {
            let cur = kc.current_context.clone();
            kc.contexts.into_iter().find(|c| c.name == cur).and_then(|c| c.context.namespace)
        })
        .unwrap_or_else(|| "default".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kubeconfig() {
        let dir = std::env::temp_dir().join(format!("kc-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config");
        std::fs::write(
            &p,
            "apiVersion: v1\nkind: Config\ncurrent-context: a\nclusters:\n- name: c\n  cluster:\n    server: http://127.0.0.1:6443/\ncontexts:\n- name: a\n  context:\n    cluster: c\n    user: u\n    namespace: dev\nusers:\n- name: u\n  user:\n    token: abc\n",
        )
        .unwrap();
        let cfg = load_config(&p, None).unwrap();
        assert_eq!(cfg.server, "http://127.0.0.1:6443");
        assert_eq!(cfg.token().as_deref(), Some("abc"));
        assert_eq!(current_namespace(&p), "dev");
        std::fs::remove_dir_all(dir).ok();
    }
}
