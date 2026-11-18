//! `kubeadm init phase kubeconfig` (`cmd/kubeadm/app/phases/kubeconfig`):
//! kubeconfig files with embedded client certificates for each component.
//!
//! | File | User (CN / O) | Bound to |
//! |------|---------------|----------|
//! | `admin.conf` | kubernetes-admin / kubeadm:cluster-admins | `cluster-admin` |
//! | `super-admin.conf` | kubernetes-super-admin / system:masters | (bypasses RBAC) |
//! | `controller-manager.conf` | system:kube-controller-manager | `system:kube-controller-manager` |
//! | `scheduler.conf` | system:kube-scheduler | `system:kube-scheduler` |
//! | `kubelet.conf` | system:node:<node> / system:nodes | `system:node` |
//! | `kube-proxy.conf` | system:kube-proxy | `system:node-proxier` |

use std::path::Path;

use anyhow::{Context, Result};
use base64::Engine;

use super::certs::{issue, Ca, LeafSpec};

fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
}

/// Renders a kubeconfig with embedded CA and client credentials.
pub fn render(server: &str, cluster: &str, user: &str, ca_pem: &str, cert_pem: &str, key_pem: &str) -> String {
    format!(
        "apiVersion: v1\nkind: Config\nclusters:\n- name: {cluster}\n  cluster:\n    server: {server}\n    certificate-authority-data: {}\ncontexts:\n- name: {user}@{cluster}\n  context:\n    cluster: {cluster}\n    user: {user}\ncurrent-context: {user}@{cluster}\nusers:\n- name: {user}\n  user:\n    client-certificate-data: {}\n    client-key-data: {}\n",
        b64(ca_pem),
        b64(cert_pem),
        b64(key_pem)
    )
}

/// A kubeconfig for an arbitrary user (`kubeadm kubeconfig user`).
pub fn user(ca: &Ca, server: &str, cluster: &str, name: &str, orgs: &[&str]) -> Result<String> {
    let (c, k) = issue(ca, &LeafSpec { cn: name, orgs, dns: vec![], ips: vec![], server: false, client: true })?;
    Ok(render(server, cluster, name, &ca.pem, &c, &k))
}

fn ensure(dir: &Path, file: &str, ca: &Ca, server: &str, cluster: &str, name: &str, orgs: &[&str]) -> Result<()> {
    let path = dir.join(file);
    if path.exists() {
        println!("[kubeconfig] Using existing kubeconfig file: {}", path.display());
        return Ok(());
    }
    let kc = user(ca, server, cluster, name, orgs)?;
    std::fs::write(&path, kc).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    println!("[kubeconfig] Writing \"{file}\" kubeconfig file");
    Ok(())
}

/// `kubeadm init phase kubeconfig all`.
pub fn all(dir: &Path, ca: &Ca, server: &str, cluster: &str, node_name: &str) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    ensure(dir, "admin.conf", ca, server, cluster, "kubernetes-admin", &["kubeadm:cluster-admins"])?;
    ensure(dir, "super-admin.conf", ca, server, cluster, "kubernetes-super-admin", &["system:masters"])?;
    ensure(dir, "controller-manager.conf", ca, server, cluster, "system:kube-controller-manager", &[])?;
    ensure(dir, "scheduler.conf", ca, server, cluster, "system:kube-scheduler", &[])?;
    ensure(dir, "kubelet.conf", ca, server, cluster, &format!("system:node:{node_name}"), &["system:nodes"])?;
    ensure(dir, "kube-proxy.conf", ca, server, cluster, "system:kube-proxy", &[])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kubeconfig_loads_with_client_go() {
        let dir = std::env::temp_dir().join(format!("kubeadm-kc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ca = super::super::certs::ensure_ca(&dir).unwrap();
        let kc = user(&ca, "https://127.0.0.1:6443", "kubernetes", "jane", &["dev"]).unwrap();
        let p = dir.join("jane.conf");
        std::fs::write(&p, kc).unwrap();
        let cfg = k8s_client_go::tools::clientcmd::load_config(&p, None).unwrap();
        assert_eq!(cfg.server, "https://127.0.0.1:6443");
        assert!(cfg.tls.ca_data.is_some() && cfg.tls.cert_data.is_some() && cfg.tls.key_data.is_some());
        cfg.tls.rustls().unwrap();
        std::fs::remove_dir_all(dir).ok();
    }
}
