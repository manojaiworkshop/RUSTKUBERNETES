//! The console's own data: users, machines, clusters and the audit trail.
//!
//! Everything lives in `<data-dir>/state.json` (written atomically, mode 0600)
//! plus `<data-dir>/audit.log` (one JSON object per line). The super admin is
//! not stored: it comes from `RUSTKUBEADMIN_USER` / `RUSTKUBEADMIN_PASSWORD`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Console permissions a super admin can grant (the super admin has all of them).
pub const PERMISSIONS: &[(&str, &str)] = &[
    ("users.manage", "Create, edit and delete console users and their cluster access"),
    ("machines.manage", "Add machines, run the offline installer, remove machines"),
    ("clusters.manage", "Create, import, start, stop and delete clusters"),
    ("audit.view", "See the activity log of every user"),
];

/// Kubernetes access of a console user to one cluster (enforced by RBAC through impersonation).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Grant {
    pub cluster: String,
    /// Empty: the whole cluster.
    #[serde(default)]
    pub namespace: String,
    /// `admin`, `edit` or `view`.
    pub role: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct User {
    pub username: String,
    /// `pbkdf2-sha256$<iterations>$<salt b64>$<hash b64>`
    pub password_hash: String,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub grants: Vec<Grant>,
    #[serde(default)]
    pub disabled: bool,
    pub created_at: String,
    pub created_by: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Machine {
    pub id: String,
    pub name: String,
    /// IP or hostname other machines (and pods) use to reach it.
    pub address: String,
    /// `local` (the machine running rustkubeadmin) or `ssh`.
    pub transport: String,
    #[serde(default)]
    pub ssh_user: String,
    #[serde(default)]
    pub ssh_port: u16,
    /// `admin-key` (rustkubeadmin's own key), `key` (a key stored for this machine) or `password`.
    #[serde(default)]
    pub ssh_auth: String,
    /// Install directory on the machine (`$HOME/.local/rustkube` by default).
    pub prefix: String,
    /// `new`, `installing`, `ready`, `failed`
    pub status: String,
    #[serde(default)]
    pub message: String,
    /// Facts reported by the installer (hostname, os, kernel, cpus, memory, docker, supervisor, ...).
    #[serde(default)]
    pub facts: serde_json::Map<String, Value>,
    #[serde(default)]
    pub installed_version: String,
    pub created_at: String,
    pub created_by: String,
}

/// How pods get their IPs and reach each other.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PodNetwork {
    /// `kubenet` (node pod CIDRs, bridge cbr0, flannel add-on) or `""` (each machine's Docker bridge).
    #[serde(default)]
    pub plugin: String,
    /// kube-controller-manager `--cluster-cidr` (flannel's Network).
    #[serde(default)]
    pub pod_cidr: String,
    /// flannel backend: `vxlan` (works across subnets; pod MTU 1450) or `host-gw` (machines on one L2 network).
    #[serde(default)]
    pub backend: String,
}

impl PodNetwork {
    pub fn kubenet(&self) -> bool {
        self.plugin == "kubenet"
    }

    /// The default for new clusters.
    pub fn recommended() -> Self {
        Self { plugin: "kubenet".into(), pod_cidr: "10.244.0.0/16".into(), backend: "vxlan".into() }
    }
}

/// Ports one cluster uses on its machines (several clusters may share a host).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ports {
    pub api: u16,
    pub kubelet: u16,
    pub controller_manager: u16,
    pub scheduler: u16,
    pub proxy_healthz: u16,
    pub proxy_metrics: u16,
    /// etcd client / peer / metrics ports (kubeadm: 2379, 2380, 2381).
    #[serde(default = "etcd_client")]
    pub etcd_client: u16,
    #[serde(default = "etcd_peer")]
    pub etcd_peer: u16,
    #[serde(default = "etcd_metrics")]
    pub etcd_metrics: u16,
}

fn etcd_client() -> u16 {
    2379
}
fn etcd_peer() -> u16 {
    2380
}
fn etcd_metrics() -> u16 {
    2381
}

impl Default for Ports {
    fn default() -> Self {
        Self {
            api: 6443,
            kubelet: 10250,
            controller_manager: 10257,
            scheduler: 10259,
            proxy_healthz: 10256,
            proxy_metrics: 10249,
            etcd_client: etcd_client(),
            etcd_peer: etcd_peer(),
            etcd_metrics: etcd_metrics(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Cluster {
    pub name: String,
    /// `managed` (provisioned by rustkubeadmin on its machines) or `imported` (a kubeconfig).
    pub kind: String,
    /// `provisioning`, `running`, `stopped`, `failed`, `deleting`
    pub status: String,
    #[serde(default)]
    pub message: String,
    pub endpoint: String,
    #[serde(default)]
    pub master: String,
    #[serde(default)]
    pub workers: Vec<String>,
    #[serde(default)]
    pub ports: Ports,
    #[serde(default)]
    pub dns: bool,
    #[serde(default)]
    pub kube_proxy: bool,
    /// The metrics-server add-on (`kubectl top`).
    #[serde(default)]
    pub metrics: bool,
    /// The local-path-provisioner add-on (default StorageClass `local-path`).
    #[serde(default)]
    pub storage: bool,
    /// Pod network. Clusters created before it existed keep Docker's default bridge.
    #[serde(default)]
    pub network: PodNetwork,
    pub created_at: String,
    pub created_by: String,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Data {
    #[serde(default)]
    pub users: Vec<User>,
    #[serde(default)]
    pub machines: Vec<Machine>,
    #[serde(default)]
    pub clusters: Vec<Cluster>,
}

pub struct Store {
    pub dir: PathBuf,
    data: Mutex<Data>,
}

pub fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

impl Store {
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let file = dir.join("state.json");
        let data = match std::fs::read(&file) {
            Ok(b) => serde_json::from_slice(&b)?,
            Err(_) => Data::default(),
        };
        Ok(Self { dir: dir.to_path_buf(), data: Mutex::new(data) })
    }

    /// Reads the data.
    pub fn read<R>(&self, f: impl FnOnce(&Data) -> R) -> R {
        f(&self.data.lock().unwrap())
    }

    /// Changes the data and saves it.
    pub fn write<R>(&self, f: impl FnOnce(&mut Data) -> R) -> R {
        let mut d = self.data.lock().unwrap();
        let r = f(&mut d);
        if let Err(e) = self.save(&d) {
            tracing::error!("saving state: {e}");
        }
        r
    }

    fn save(&self, d: &Data) -> std::io::Result<()> {
        let tmp = self.dir.join("state.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(d)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(tmp, self.dir.join("state.json"))
    }

    pub fn machine(&self, id: &str) -> Option<Machine> {
        self.read(|d| d.machines.iter().find(|m| m.id == id).cloned())
    }

    pub fn cluster(&self, name: &str) -> Option<Cluster> {
        self.read(|d| d.clusters.iter().find(|c| c.name == name).cloned())
    }

    pub fn update_machine(&self, id: &str, f: impl FnOnce(&mut Machine)) {
        self.write(|d| {
            if let Some(m) = d.machines.iter_mut().find(|m| m.id == id) {
                f(m);
            }
        });
    }

    pub fn update_cluster(&self, name: &str, f: impl FnOnce(&mut Cluster)) {
        self.write(|d| {
            if let Some(c) = d.clusters.iter_mut().find(|c| c.name == name) {
                f(c);
            }
        });
    }

    /// Appends one entry to the audit log.
    pub fn audit(&self, user: &str, action: &str, detail: &str) {
        use std::io::Write;
        let line = json!({"time": now(), "user": user, "action": action, "detail": detail}).to_string();
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join("audit.log")) {
            let _ = writeln!(f, "{line}");
        }
    }

    /// The newest `limit` audit entries, newest first.
    pub fn audit_tail(&self, limit: usize) -> Vec<Value> {
        let raw = std::fs::read_to_string(self.dir.join("audit.log")).unwrap_or_default();
        raw.lines().rev().take(limit).filter_map(|l| serde_json::from_str(l).ok()).collect()
    }

    /// Per-cluster directory (PKI, kubeconfigs, generated service files).
    pub fn cluster_dir(&self, name: &str) -> PathBuf {
        self.dir.join("clusters").join(name)
    }
}

/// Lowercase DNS-label form of a name (`My Box_1` → `my-box-1`).
pub fn dns_label(s: &str) -> String {
    let mut out: String = s.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').chars().take(63).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_audit() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        s.write(|d| d.machines.push(Machine { id: "m1".into(), name: "box".into(), status: "new".into(), ..Default::default() }));
        s.audit("alice", "machine.add", "box");
        let s2 = Store::open(dir.path()).unwrap();
        assert_eq!(s2.machine("m1").unwrap().name, "box");
        assert_eq!(s2.audit_tail(5)[0]["action"], "machine.add");
        assert_eq!(dns_label("My Box_1"), "my-box-1");
    }
}
