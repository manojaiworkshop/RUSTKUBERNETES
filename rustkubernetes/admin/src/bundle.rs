//! The offline bundle: everything a machine needs, in one tarball, so machines
//! never download anything (air-gapped installs).
//!
//! ```text
//! rustkube-bundle/
//!   VERSION, ARCH
//!   install.sh                 assets/install.sh
//!   bin/kube-apiserver kube-controller-manager kube-scheduler kubelet kube-proxy kubeadm rkubectl rustkube-node
//!   images/pause.tar coredns.tar kube-proxy-base.tar     (docker save)
//! ```
//!
//! It is rebuilt only when a binary or image changed (fingerprint of sizes,
//! modification times and image IDs).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::jobs::JobHandle;

pub const BINARIES: &[&str] =
    &["kube-apiserver", "kube-controller-manager", "kube-scheduler", "kubelet", "kube-proxy", "kubeadm", "rkubectl"];
pub const IMAGES: &[(&str, &str)] = &[
    ("pause", "registry.k8s.io/pause:3.10"),
    ("coredns", "registry.k8s.io/coredns/coredns:v1.12.1"),
    ("kube-proxy-base", "rustkubernetes/kube-proxy-base:24.04"),
];
pub const KUBE_PROXY_IMAGE: &str = "rustkubernetes/kube-proxy-base:24.04";

const INSTALL_SH: &str = include_str!("../assets/install.sh");
const NODE_SH: &str = include_str!("../assets/rustkube-node");

pub struct Bundle {
    pub binaries: PathBuf,
    pub dir: PathBuf,
}

async fn sh(cmd: &str, args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new(cmd).args(args).output().await.with_context(|| format!("running {cmd}"))?;
    if !out.status.success() {
        bail!("{cmd} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl Bundle {
    pub fn tarball(&self) -> PathBuf {
        self.dir.join("rustkube-bundle.tar.gz")
    }

    fn info_path(&self) -> PathBuf {
        self.dir.join("bundle.json")
    }

    pub fn info(&self) -> Value {
        let mut v: Value = std::fs::read(self.info_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(json!({}));
        v["binaries_dir"] = json!(self.binaries.display().to_string());
        v["built"] = json!(self.tarball().exists());
        v
    }

    /// Binaries present / missing in the binaries directory.
    pub fn missing_binaries(&self) -> Vec<&'static str> {
        BINARIES.iter().copied().filter(|b| !self.binaries.join(b).is_file()).collect()
    }

    async fn fingerprint(&self) -> Result<String> {
        let mut parts = vec![env!("CARGO_PKG_VERSION").to_string(), format!("{:x}", INSTALL_SH.len() + NODE_SH.len())];
        for b in BINARIES {
            let m = std::fs::metadata(self.binaries.join(b)).with_context(|| format!("{b} not found in {}", self.binaries.display()))?;
            let t = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
            parts.push(format!("{b}:{}:{t}", m.len()));
        }
        for (_, img) in IMAGES {
            parts.push(sh("docker", &["image", "inspect", "-f", "{{.Id}}", img]).await.unwrap_or_else(|_| "missing".into()));
        }
        let d = ring::digest::digest(&ring::digest::SHA256, parts.join("\n").as_bytes());
        Ok(d.as_ref()[..6].iter().map(|b| format!("{b:02x}")).collect())
    }

    /// Builds the bundle if needed and returns its path.
    pub async fn ensure(&self, job: &JobHandle) -> Result<PathBuf> {
        let missing = self.missing_binaries();
        if !missing.is_empty() {
            bail!(
                "binaries missing in {}: {} (build them with `cargo build --release` or pass --binaries)",
                self.binaries.display(),
                missing.join(", ")
            );
        }
        let fp = self.fingerprint().await?;
        if self.tarball().exists() && self.info()["fingerprint"] == json!(fp) {
            job.log(format!("offline bundle is up to date ({})", self.info()["size_human"].as_str().unwrap_or("?")));
            return Ok(self.tarball());
        }
        job.log(format!("building the offline bundle from {}", self.binaries.display()));
        let stage = self.dir.join("stage");
        let root = stage.join("rustkube-bundle");
        if stage.exists() {
            std::fs::remove_dir_all(&stage)?;
        }
        std::fs::create_dir_all(root.join("bin"))?;
        std::fs::create_dir_all(root.join("images"))?;
        for b in BINARIES {
            std::fs::copy(self.binaries.join(b), root.join("bin").join(b))?;
        }
        write_exec(&root.join("bin/rustkube-node"), NODE_SH)?;
        write_exec(&root.join("install.sh"), INSTALL_SH)?;
        let version = format!("{}-{fp}", env!("CARGO_PKG_VERSION"));
        std::fs::write(root.join("VERSION"), format!("{version}\n"))?;
        std::fs::write(root.join("ARCH"), format!("{}\n", std::env::consts::ARCH))?;
        for (name, img) in IMAGES {
            if sh("docker", &["image", "inspect", img]).await.is_err() {
                job.log(format!("  image {img} is not on this machine; trying to pull it once"));
                if let Err(e) = sh("docker", &["pull", "-q", img]).await {
                    job.log(format!("  warning: {e} — the bundle will not contain {img}"));
                    continue;
                }
            }
            job.log(format!("  saving image {img}"));
            let path = root.join("images").join(format!("{name}.tar"));
            sh("docker", &["save", "-o", path.to_str().unwrap(), img]).await?;
        }
        job.log("  compressing");
        let tmp = self.dir.join("rustkube-bundle.tar.gz.tmp");
        sh("tar", &["-czf", tmp.to_str().unwrap(), "-C", stage.to_str().unwrap(), "rustkube-bundle"]).await?;
        std::fs::rename(&tmp, self.tarball())?;
        std::fs::remove_dir_all(&stage)?;
        let size = std::fs::metadata(self.tarball())?.len();
        let info = json!({
            "version": version,
            "fingerprint": fp,
            "created": crate::state::now(),
            "size": size,
            "size_human": human(size),
            "binaries": BINARIES,
            "images": IMAGES.iter().map(|(_, i)| *i).collect::<Vec<_>>(),
            "arch": std::env::consts::ARCH,
        });
        std::fs::write(self.info_path(), serde_json::to_vec_pretty(&info)?)?;
        job.log(format!("offline bundle ready: {version}, {}", human(size)));
        Ok(self.tarball())
    }
}

fn write_exec(path: &Path, body: &str) -> Result<()> {
    std::fs::write(path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

pub fn human(n: u64) -> String {
    let mb = n as f64 / 1_048_576.0;
    if mb >= 1024.0 {
        format!("{:.1} GiB", mb / 1024.0)
    } else {
        format!("{mb:.0} MiB")
    }
}
