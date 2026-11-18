//! How rustkubeadmin reaches a machine: run a shell script there and copy files to it.
//!
//! * `local`: the machine running rustkubeadmin (`sh -s`, `cp`).
//! * `ssh`: OpenSSH (`ssh … sh -s`, `scp`) with rustkubeadmin's own key, a key stored
//!   for the machine, or a password (via `sshpass`). Host keys are remembered in
//!   `<data-dir>/ssh/known_hosts` on first contact (`StrictHostKeyChecking=accept-new`).
//!
//! Scripts travel on stdin, so nothing needs shell quoting; their output is
//! streamed line by line into the job log.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::jobs::JobHandle;
use crate::state::Machine;

#[derive(Clone, Debug)]
pub enum Auth {
    Key(PathBuf),
    Password(String),
}

#[derive(Clone, Debug)]
pub enum Target {
    Local,
    Ssh { user: String, host: String, port: u16, auth: Auth, known_hosts: PathBuf },
}

/// Paths of the SSH material in the data directory.
pub struct SshPaths {
    pub dir: PathBuf,
}

impl SshPaths {
    pub fn admin_key(&self) -> PathBuf {
        self.dir.join("id_ed25519")
    }
    pub fn machine_key(&self, id: &str) -> PathBuf {
        self.dir.join("machines").join(format!("{id}.key"))
    }
    pub fn machine_password(&self, id: &str) -> PathBuf {
        self.dir.join("machines").join(format!("{id}.pass"))
    }
    pub fn known_hosts(&self) -> PathBuf {
        self.dir.join("known_hosts")
    }

    /// Creates rustkubeadmin's own key pair on first use; returns the public key line.
    pub async fn ensure_admin_key(&self) -> Result<String> {
        std::fs::create_dir_all(self.dir.join("machines"))?;
        let key = self.admin_key();
        if !key.exists() {
            let out = tokio::process::Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-C", "rustkubeadmin", "-f"])
                .arg(&key)
                .output()
                .await
                .context("running ssh-keygen (is OpenSSH installed?)")?;
            if !out.status.success() {
                bail!("ssh-keygen failed: {}", String::from_utf8_lossy(&out.stderr));
            }
        }
        Ok(std::fs::read_to_string(key.with_extension("pub"))?.trim().to_string())
    }

    /// Stores a secret readable only by rustkubeadmin.
    pub fn store_secret(path: &Path, data: &str) -> Result<()> {
        std::fs::create_dir_all(path.parent().unwrap())?;
        let mut body = data.to_string();
        if !body.ends_with('\n') && body.contains("PRIVATE KEY") {
            body.push('\n');
        }
        std::fs::write(path, body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn target(&self, m: &Machine) -> Result<Target> {
        if m.transport == "local" {
            return Ok(Target::Local);
        }
        let auth = match m.ssh_auth.as_str() {
            "password" => Auth::Password(
                std::fs::read_to_string(self.machine_password(&m.id)).context("stored SSH password is missing")?.trim_end().to_string(),
            ),
            "key" => Auth::Key(self.machine_key(&m.id)),
            _ => Auth::Key(self.admin_key()),
        };
        Ok(Target::Ssh {
            user: m.ssh_user.clone(),
            host: m.address.clone(),
            port: if m.ssh_port == 0 { 22 } else { m.ssh_port },
            auth,
            known_hosts: self.known_hosts(),
        })
    }
}

impl Target {
    fn ssh_options(&self) -> Vec<String> {
        let Target::Ssh { port, auth, known_hosts, .. } = self else { return vec![] };
        let mut o = vec![
            "-o".into(),
            "StrictHostKeyChecking=accept-new".into(),
            "-o".into(),
            format!("UserKnownHostsFile={}", known_hosts.display()),
            "-o".into(),
            "ConnectTimeout=10".into(),
            "-o".into(),
            "ServerAliveInterval=15".into(),
        ];
        match auth {
            Auth::Key(k) => {
                o.extend([
                    "-o".into(),
                    "BatchMode=yes".into(),
                    "-o".into(),
                    "IdentitiesOnly=yes".into(),
                    "-i".into(),
                    k.display().to_string(),
                ]);
            }
            Auth::Password(_) => {
                o.extend([
                    "-o".into(),
                    "PubkeyAuthentication=no".into(),
                    "-o".into(),
                    "PreferredAuthentications=password,keyboard-interactive".into(),
                ]);
            }
        }
        let _ = port;
        o
    }

    fn base(&self, program: &str) -> tokio::process::Command {
        match self {
            Target::Ssh { auth: Auth::Password(p), .. } => {
                let mut c = tokio::process::Command::new("sshpass");
                c.arg("-e").arg(program).env("SSHPASS", p);
                c
            }
            _ => tokio::process::Command::new(program),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Target::Local => "this machine".into(),
            Target::Ssh { user, host, port, .. } => format!("{user}@{host}:{port}"),
        }
    }

    /// Runs `script` with `sh -s`; streams output into `job` and returns stdout.
    pub async fn run(&self, script: &str, job: Option<&JobHandle>) -> Result<String> {
        let mut cmd = match self {
            Target::Local => {
                let mut c = tokio::process::Command::new("sh");
                c.arg("-s");
                c
            }
            Target::Ssh { user, host, port, .. } => {
                let mut c = self.base("ssh");
                c.args(self.ssh_options()).arg("-p").arg(port.to_string()).arg(format!("{user}@{host}")).args(["sh", "-s"]);
                c
            }
        };
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let mut child = cmd.spawn().with_context(|| format!("starting a shell on {}", self.describe()))?;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(script.as_bytes()).await?;
        drop(stdin);
        let out = child.stdout.take().unwrap();
        let err = child.stderr.take().unwrap();
        let j2 = job.cloned();
        let err_task = tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            let mut all = String::new();
            while let Ok(Some(l)) = lines.next_line().await {
                if let Some(j) = &j2 {
                    j.log(format!("  {l}"));
                }
                all.push_str(&l);
                all.push('\n');
            }
            all
        });
        let mut stdout = String::new();
        let mut lines = BufReader::new(out).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if let Some(j) = job {
                if !l.starts_with("FACT ") {
                    j.log(format!("  {l}"));
                }
            }
            stdout.push_str(&l);
            stdout.push('\n');
        }
        let status = child.wait().await?;
        let stderr = err_task.await.unwrap_or_default();
        if !status.success() {
            let tail: Vec<&str> = stderr.lines().rev().take(3).collect();
            let hint = if stderr.contains("Permission denied (publickey") {
                " — add rustkubeadmin's public key (Settings) to ~/.ssh/authorized_keys on the machine"
            } else {
                ""
            };
            bail!("command on {} exited with {status}: {}{hint}", self.describe(), tail.into_iter().rev().collect::<Vec<_>>().join(" | "));
        }
        Ok(stdout)
    }

    /// Copies a local file to `remote` (absolute path, or relative to the login directory).
    pub async fn copy(&self, local: &Path, remote: &str) -> Result<()> {
        match self {
            Target::Local => {
                let dest = expand_home(remote);
                if let Some(p) = Path::new(&dest).parent() {
                    std::fs::create_dir_all(p)?;
                }
                std::fs::copy(local, &dest).with_context(|| format!("copying {} to {dest}", local.display()))?;
                Ok(())
            }
            Target::Ssh { user, host, port, .. } => {
                let out = self
                    .base("scp")
                    .args(self.ssh_options())
                    .arg("-q")
                    .arg("-P")
                    .arg(port.to_string())
                    .arg(local)
                    .arg(format!("{user}@{host}:{remote}"))
                    .output()
                    .await
                    .context("running scp")?;
                if !out.status.success() {
                    bail!("copying to {}: {}", self.describe(), String::from_utf8_lossy(&out.stderr).trim());
                }
                Ok(())
            }
        }
    }
}

/// `$HOME/...` → absolute path (for the local transport).
pub fn expand_home(p: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    p.replace("$HOME", &home).replacen("~/", &format!("{home}/"), 1)
}

/// Single-quotes a value for a POSIX shell.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_run_and_copy() {
        let t = Target::Local;
        let out = t.run("echo hello; echo FACT os=test; echo oops >&2", None).await.unwrap();
        assert!(out.contains("hello") && out.contains("FACT os=test"));
        assert!(t.run("exit 3", None).await.unwrap_err().to_string().contains("exited"));
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.txt");
        std::fs::write(&src, "x").unwrap();
        let dst = dir.path().join("sub/b.txt");
        t.copy(&src, dst.to_str().unwrap()).await.unwrap();
        assert_eq!(std::fs::read_to_string(dst).unwrap(), "x");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }
}
