//! Machines: adding, the offline install, connectivity checks, removal.

use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::jobs::JobHandle;
use crate::transport::{sh_quote, Target};
use crate::App;

/// The address other machines and pods use for this host (source IP of the default route).
pub async fn local_address() -> String {
    let out = tokio::process::Command::new("ip").args(["-4", "route", "get", "1.1.1.1"]).output().await;
    if let Ok(o) = out {
        let s = String::from_utf8_lossy(&o.stdout).to_string();
        let mut it = s.split_whitespace();
        while let Some(w) = it.next() {
            if w == "src" {
                if let Some(ip) = it.next() {
                    return ip.to_string();
                }
            }
        }
    }
    "127.0.0.1".into()
}

/// Validates an install prefix (absolute path or `$HOME/...`, no shell metacharacters).
pub fn check_prefix(p: &str) -> Result<()> {
    if !(p.starts_with('/') || p.starts_with("$HOME/")) {
        bail!("the install directory must be absolute or start with $HOME/");
    }
    if !p.chars().all(|c| c.is_ascii_alphanumeric() || "/._-$".contains(c)) || p.matches('$').count() > 1 {
        bail!("the install directory may only contain letters, digits, / . _ - (and a leading $HOME)");
    }
    Ok(())
}

pub fn publish(app: &App) {
    let list: Vec<Value> = app.store.read(|d| {
        d.machines
            .iter()
            .map(|m| {
                let cluster = d.clusters.iter().find(|c| c.master == m.id || c.workers.contains(&m.id));
                json!({
                    "id": m.id, "name": m.name, "address": m.address, "transport": m.transport,
                    "sshUser": m.ssh_user, "sshPort": m.ssh_port, "sshAuth": m.ssh_auth, "prefix": m.prefix,
                    "status": m.status, "message": m.message, "facts": m.facts, "version": m.installed_version,
                    "created": m.created_at, "createdBy": m.created_by,
                    "cluster": cluster.map(|c| c.name.clone()),
                    "role": cluster.map(|c| if c.master == m.id { "master" } else { "worker" }),
                })
            })
            .collect()
    });
    app.hub.publish("machines", Value::Array(list));
}

/// `install` job: copy the offline bundle to the machine and run its installer.
pub async fn install(app: Arc<App>, id: String, job: JobHandle) -> Result<()> {
    let m = app.store.machine(&id).ok_or_else(|| anyhow!("machine not found"))?;
    app.store.update_machine(&id, |m| {
        m.status = "installing".into();
        m.message.clear();
    });
    publish(&app);
    let r = install_inner(&app, &m, &job).await;
    app.store.update_machine(&id, |m| match &r {
        Ok(facts) => {
            m.status = "ready".into();
            m.installed_version = facts.get("version").and_then(Value::as_str).unwrap_or("").to_string();
            m.facts = facts.clone();
        }
        Err(e) => {
            m.status = "failed".into();
            m.message = format!("{e:#}");
        }
    });
    publish(&app);
    r.map(|_| ())
}

async fn install_inner(app: &App, m: &crate::state::Machine, job: &JobHandle) -> Result<serde_json::Map<String, Value>> {
    let target = app.ssh.target(m)?;
    job.log(format!("connecting to {} ({})", m.name, target.describe()));
    let hello = target.run("echo \"$(uname -sm) as $(id -un)\"\n", None).await?;
    job.log(format!("  {}", hello.trim()));
    let bundle = app.bundle.ensure(job).await?;
    let remote = format!("/tmp/rustkube-bundle-{}.tar.gz", job.id);
    job.log(format!("copying the offline bundle ({})", crate::bundle::human(std::fs::metadata(&bundle)?.len())));
    target.copy(&bundle, &remote).await?;
    job.log(format!("running the installer into {}", m.prefix));
    let prefix =
        if m.prefix.starts_with("$HOME/") { format!("\"$HOME\"{}", sh_quote(&m.prefix["$HOME".len()..])) } else { sh_quote(&m.prefix) };
    let script = format!(
        "d=$(mktemp -d)\ntrap 'rm -rf \"$d\" {remote}' EXIT\ntar -xzf {remote} -C \"$d\"\nsh \"$d/rustkube-bundle/install.sh\" {prefix}\n"
    );
    let out = target.run(&script, Some(job)).await?;
    let mut facts = serde_json::Map::new();
    for l in out.lines() {
        if let Some((k, v)) = l.strip_prefix("FACT ").and_then(|f| f.split_once('=')) {
            facts.insert(k.to_string(), json!(v));
        }
    }
    if !facts.contains_key("prefix") {
        bail!("the installer did not report its facts");
    }
    job.log(format!(
        "{}: {} · {} CPUs · Docker {} · supervisor {}",
        m.name,
        facts.get("os").and_then(Value::as_str).unwrap_or("?"),
        facts.get("cpus").and_then(Value::as_str).unwrap_or("?"),
        facts.get("docker").and_then(Value::as_str).unwrap_or("?"),
        facts.get("supervisor").and_then(Value::as_str).unwrap_or("?")
    ));
    Ok(facts)
}

/// Quick reachability test (`echo ok` over the transport).
pub async fn test(app: &App, id: &str) -> Result<String> {
    let m = app.store.machine(id).ok_or_else(|| anyhow!("machine not found"))?;
    let t: Target = app.ssh.target(&m)?;
    let out = t
        .run("echo \"$(hostname) $(uname -r) docker=$(docker version -f '{{.Server.Version}}' 2>/dev/null || echo none)\"\n", None)
        .await?;
    Ok(out.trim().to_string())
}

/// Uninstalls rustkubernetes from a machine (best effort) — only when no cluster uses it.
pub async fn uninstall(app: &App, id: &str) -> Result<()> {
    let m = app.store.machine(id).ok_or_else(|| anyhow!("machine not found"))?;
    if let Some(prefix) = m.facts.get("prefix").and_then(Value::as_str) {
        if let Ok(t) = app.ssh.target(&m) {
            let script = format!("rm -rf {}/bin {}/VERSION {}/SUPERVISOR\n", sh_quote(prefix), sh_quote(prefix), sh_quote(prefix));
            let _ = t.run(&script, None).await;
        }
    }
    Ok(())
}
