//! Docker implementation of [`Runtime`] (the approach of upstream's former
//! `dockershim` and today's `cri-dockerd`).
//!
//! * sandbox = a `pause` container that owns the pod's network namespace
//!   (bridge network, or the host network for `hostNetwork` pods);
//! * app containers use `NetworkMode/IpcMode = container:<sandbox>` so all
//!   containers of a pod share localhost and the pod IP;
//! * names follow dockershim: `k8s_<container>_<pod>_<namespace>_<uid>_<attempt>`;
//! * the pod network comes from a [`network`] plugin (`--network-plugin`).

pub mod client;
pub mod network;

use std::collections::BTreeMap;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures::StreamExt;
use hyper::Method;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

use crate::runtime::*;
use client::{is_not_found, Demuxer, DockerClient};

pub struct DockerRuntime {
    client: DockerClient,
    node: String,
    pause_image: String,
    network: network::Plugin,
    /// Per-sandbox files written by the shim (resolv.conf).
    state_dir: std::path::PathBuf,
    /// Static `rustkube-portforward` helper (see [`Runtime::port_forward`]).
    portforward_helper: Option<std::path::PathBuf>,
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn str_map(v: &Value) -> BTreeMap<String, String> {
    v.as_object().map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect()).unwrap_or_default()
}

/// Docker reports zero times as `0001-01-01T00:00:00Z`.
fn time(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.starts_with("0001-")).map(|s| {
        chrono::DateTime::parse_from_rfc3339(s)
            .map(|t| t.with_timezone(&chrono::Utc).to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            .unwrap_or_else(|_| s.to_string())
    })
}

/// Docker stats → [`ContainerStats`]. cgroup v2 reports `inactive_file`, v1 `total_inactive_file`.
pub fn parse_stats(v: &Value) -> ContainerStats {
    let usage = v["memory_stats"]["usage"].as_u64().unwrap_or(0);
    let st = &v["memory_stats"]["stats"];
    let inactive = st["inactive_file"].as_u64().or(st["total_inactive_file"].as_u64()).unwrap_or(0);
    let ts = v["read"]
        .as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .filter(|t| t.timestamp() > 0)
        .map(|t| t.timestamp_millis())
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
    ContainerStats {
        timestamp_ms: ts,
        cpu_usage_ns: v["cpu_stats"]["cpu_usage"]["total_usage"].as_u64().unwrap_or(0),
        memory_working_set_bytes: usage.saturating_sub(inactive),
    }
}

/// A ustar archive with one empty file (mode 0644) or directory (0755).
fn tar_entry(name: &str, dir: bool) -> Vec<u8> {
    let mut h = [0u8; 512];
    let n = name.as_bytes();
    h[..n.len().min(100)].copy_from_slice(&n[..n.len().min(100)]);
    let field = |h: &mut [u8; 512], off: usize, len: usize, v: &str| h[off..off + v.len().min(len)].copy_from_slice(v.as_bytes());
    field(&mut h, 100, 8, if dir { "0000755\0" } else { "0000644\0" });
    field(&mut h, 108, 8, "0000000\0");
    field(&mut h, 116, 8, "0000000\0");
    field(&mut h, 124, 12, "00000000000\0");
    let mtime = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    field(&mut h, 136, 12, &format!("{mtime:011o}\0"));
    h[156] = if dir { b'5' } else { b'0' };
    field(&mut h, 257, 6, "ustar\0");
    field(&mut h, 263, 2, "00");
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|b| *b as u32).sum();
    field(&mut h, 148, 8, &format!("{sum:06o}\0 "));
    let mut out = h.to_vec();
    out.extend_from_slice(&[0u8; 1024]);
    out
}

/// `repo[:tag][@digest]` → (`fromImage`, `tag`) for `POST /images/create`.
pub fn split_image(image: &str) -> (String, String) {
    if image.contains('@') {
        return (image.to_string(), String::new());
    }
    let last_slash = image.rfind('/').map(|i| i + 1).unwrap_or(0);
    match image[last_slash..].rfind(':') {
        Some(i) => (image[..last_slash + i].to_string(), image[last_slash + i + 1..].to_string()),
        None => (image.to_string(), "latest".to_string()),
    }
}

impl DockerRuntime {
    pub fn new(endpoint: &str, node: &str, pause_image: &str) -> Self {
        Self {
            client: DockerClient::new(endpoint),
            node: node.into(),
            pause_image: pause_image.into(),
            network: network::Plugin::Noop,
            state_dir: std::env::temp_dir().join(format!("rustkube-dockershim-{node}")),
            portforward_helper: None,
        }
    }

    /// The static helper that connects to `localhost:PORT` inside a pod (`rustkube-portforward`).
    pub fn with_portforward_helper(mut self, path: Option<std::path::PathBuf>) -> Self {
        self.portforward_helper = path;
        self
    }

    /// Uses a network plugin (`--network-plugin`) and keeps per-sandbox files under `state_dir`.
    pub fn with_network(mut self, plugin: network::Plugin, state_dir: std::path::PathBuf) -> Self {
        self.network = plugin;
        self.state_dir = state_dir;
        self
    }

    fn sandbox_dir(&self, id: &str) -> std::path::PathBuf {
        self.state_dir.join("sandboxes").join(id)
    }

    async fn ensure_image(&self, image: &str) -> Result<()> {
        if !self.image_present(image).await? {
            self.pull_image(image).await?;
        }
        Ok(())
    }

    async fn inspect(&self, id: &str) -> Result<Value> {
        self.client.call(Method::GET, &format!("/containers/{id}/json"), None).await
    }

    fn label_filter(&self, kind: &str) -> String {
        enc(&json!({"label": [format!("{}={}", labels::NODE, self.node), format!("{}={kind}", labels::TYPE)]}).to_string())
    }

    async fn list_ids(&self, kind: &str) -> Result<Vec<String>> {
        let list = self.client.call(Method::GET, &format!("/containers/json?all=1&filters={}", self.label_filter(kind)), None).await?;
        Ok(list.as_array().into_iter().flatten().filter_map(|c| c["Id"].as_str().map(String::from)).collect())
    }

    async fn exec_create(&self, id: &str, cmd: &[String], tty: bool, stdin: bool) -> Result<String> {
        let r = self
            .client
            .call(
                Method::POST,
                &format!("/containers/{id}/exec"),
                Some(&json!({"AttachStdin": stdin, "AttachStdout": true, "AttachStderr": true, "Tty": tty, "Cmd": cmd})),
            )
            .await?;
        r["Id"].as_str().map(String::from).ok_or_else(|| anyhow!("docker exec create returned no id"))
    }
}

#[async_trait]
impl Runtime for DockerRuntime {
    async fn version(&self) -> Result<String> {
        let v = self.client.call(Method::GET, "/version", None).await?;
        Ok(format!("docker://{}", v["Version"].as_str().unwrap_or("unknown")))
    }

    async fn status(&self) -> Result<Vec<RuntimeCondition>> {
        let runtime = match self.client.call(Method::GET, "/version", None).await {
            Ok(_) => RuntimeCondition::ok(RUNTIME_READY),
            Err(e) => RuntimeCondition::failed(RUNTIME_READY, "DockerDaemonNotReady", format!("docker: failed to get docker version: {e}")),
        };
        let network = match self.network.status() {
            Ok(()) => RuntimeCondition::ok(NETWORK_READY),
            Err(e) => RuntimeCondition::failed(NETWORK_READY, "NetworkPluginNotReady", format!("docker: network plugin is not ready: {e}")),
        };
        Ok(vec![runtime, network])
    }

    /// The Docker daemon creates the path: a stopped helper container with the
    /// parent directory bind-mounted receives a one-entry tar archive.
    async fn ensure_host_path(&self, path: &std::path::Path, dir: bool) -> Result<()> {
        let parent = path.parent().filter(|p| p.is_absolute()).ok_or_else(|| anyhow!("hostPath {} has no parent", path.display()))?;
        let name = path.file_name().ok_or_else(|| anyhow!("hostPath {} has no file name", path.display()))?.to_string_lossy();
        self.ensure_image(&self.pause_image).await?;
        let body = json!({
            "Image": self.pause_image,
            "Labels": {labels::NODE: self.node},
            "HostConfig": {"Binds": [format!("{}:/host", parent.display())], "NetworkMode": "none"},
        });
        let r = self.client.call(Method::POST, "/containers/create", Some(&body)).await?;
        let id = r["Id"].as_str().ok_or_else(|| anyhow!("no helper container id"))?.to_string();
        let res = self
            .client
            .call_raw(Method::PUT, &format!("/containers/{id}/archive?path=/host"), "application/x-tar", tar_entry(&name, dir).into())
            .await;
        let _ = self.client.call(Method::DELETE, &format!("/containers/{id}?force=1"), None).await;
        res?;
        tracing::info!(path = %path.display(), dir, "created hostPath through the container runtime");
        Ok(())
    }

    async fn update_runtime_config(&self, pod_cidr: &str) -> Result<()> {
        if pod_cidr.is_empty() {
            return Ok(());
        }
        tracing::info!(pod_cidr, "Docker cri received runtime config");
        self.network.pod_cidr_changed(&self.client, pod_cidr).await
    }

    async fn run_pod_sandbox(&self, cfg: &SandboxConfig) -> Result<String> {
        if !cfg.host_network {
            if let Err(e) = self.network.status() {
                return Err(anyhow!("docker: network plugin is not ready: {e}"));
            }
        }
        self.ensure_image(&self.pause_image).await?;
        let mut lbl = cfg.labels.clone();
        lbl.insert(labels::POD_UID.into(), cfg.pod_uid.clone());
        lbl.insert(labels::POD_NAME.into(), cfg.name.clone());
        lbl.insert(labels::POD_NAMESPACE.into(), cfg.namespace.clone());
        lbl.insert(labels::CONTAINER_NAME.into(), "POD".into());
        lbl.insert(labels::TYPE.into(), labels::TYPE_SANDBOX.into());
        lbl.insert(labels::ATTEMPT.into(), cfg.attempt.to_string());
        lbl.insert(labels::NODE.into(), self.node.clone());
        let mut exposed = serde_json::Map::new();
        let mut bindings = serde_json::Map::new();
        for p in cfg.port_mappings.iter().filter(|p| p.host_port > 0) {
            let key = format!("{}/{}", p.container_port, p.protocol.to_lowercase());
            exposed.insert(key.clone(), json!({}));
            bindings.insert(key, json!([{"HostIp": p.host_ip, "HostPort": p.host_port.to_string()}]));
        }
        let mut body = json!({
            "Image": self.pause_image,
            "Hostname": if cfg.host_network { Value::Null } else { json!(cfg.hostname) },
            "Labels": lbl,
            "ExposedPorts": exposed,
            "HostConfig": {
                "NetworkMode": if cfg.host_network { "host" } else { self.network.network_mode() },
                "IpcMode": "shareable",
                "PortBindings": bindings,
                "RestartPolicy": {"Name": "no"},
            },
        });
        if let Some(d) = &cfg.dns {
            // Containers sharing the sandbox's network namespace also share its resolv.conf.
            body["HostConfig"]["Dns"] = json!(d.servers);
            body["HostConfig"]["DnsSearch"] = json!(d.searches);
            body["HostConfig"]["DnsOptions"] = json!(d.options);
        }
        let name = format!("k8s_POD_{}_{}_{}_{}", cfg.name, cfg.namespace, cfg.pod_uid, cfg.attempt);
        let r = self.client.call(Method::POST, &format!("/containers/create?name={}", enc(&name)), Some(&body)).await?;
        let id = r["Id"].as_str().ok_or_else(|| anyhow!("no sandbox id"))?.to_string();
        // On a user-defined Docker network the runtime's resolv.conf points at Docker's embedded
        // DNS; write the pod's own file and mount it into every container instead (like CRI runtimes).
        if let (false, Some(d), network::Plugin::Kubenet(_)) = (cfg.host_network, &cfg.dns, &self.network) {
            let dir = self.sandbox_dir(&id);
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("resolv.conf"), network::resolv_conf(&d.servers, &d.searches, &d.options))?;
        }
        self.client.call(Method::POST, &format!("/containers/{id}/start"), None).await?;
        Ok(id)
    }

    async fn stop_pod_sandbox(&self, id: &str) -> Result<()> {
        match self.client.call(Method::POST, &format!("/containers/{id}/stop?t=0"), None).await {
            Ok(_) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) if e.to_string().contains("304") => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn remove_pod_sandbox(&self, id: &str) -> Result<()> {
        self.remove_container(id).await?;
        let _ = std::fs::remove_dir_all(self.sandbox_dir(id));
        Ok(())
    }

    async fn list_pod_sandboxes(&self) -> Result<Vec<SandboxStatus>> {
        let mut out = vec![];
        for id in self.list_ids(labels::TYPE_SANDBOX).await? {
            let Ok(c) = self.inspect(&id).await else { continue };
            let lbl = str_map(&c["Config"]["Labels"]);
            let nets = &c["NetworkSettings"]["Networks"];
            let ip = nets[self.network.network_mode()]["IPAddress"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| nets.as_object()?.values().filter_map(|n| n["IPAddress"].as_str()).find(|s| !s.is_empty()))
                .or(c["NetworkSettings"]["IPAddress"].as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
            out.push(SandboxStatus {
                id,
                pod_uid: lbl.get(labels::POD_UID).cloned().unwrap_or_default(),
                attempt: lbl.get(labels::ATTEMPT).and_then(|a| a.parse().ok()).unwrap_or(0),
                state: if c["State"]["Running"] == true { SandboxState::Ready } else { SandboxState::NotReady },
                ip,
                created_at: time(&c["Created"]).unwrap_or_default(),
                labels: lbl,
            });
        }
        Ok(out)
    }

    async fn create_container(&self, sandbox: &SandboxStatus, cfg: &ContainerConfig) -> Result<String> {
        let mut lbl = cfg.labels.clone();
        let pod_name = sandbox.labels.get(labels::POD_NAME).cloned().unwrap_or_default();
        let pod_ns = sandbox.labels.get(labels::POD_NAMESPACE).cloned().unwrap_or_default();
        lbl.insert(labels::POD_UID.into(), sandbox.pod_uid.clone());
        lbl.insert(labels::POD_NAME.into(), pod_name.clone());
        lbl.insert(labels::POD_NAMESPACE.into(), pod_ns.clone());
        lbl.insert(labels::CONTAINER_NAME.into(), cfg.name.clone());
        lbl.insert(labels::TYPE.into(), labels::TYPE_CONTAINER.into());
        lbl.insert(labels::SANDBOX_ID.into(), sandbox.id.clone());
        lbl.insert(labels::ATTEMPT.into(), cfg.attempt.to_string());
        lbl.insert(labels::NODE.into(), self.node.clone());
        let user = match (cfg.run_as_user, cfg.run_as_group) {
            (Some(u), Some(g)) => json!(format!("{u}:{g}")),
            (Some(u), None) => json!(u.to_string()),
            _ => Value::Null,
        };
        let mut binds: Vec<String> =
            cfg.mounts.iter().map(|m| format!("{}:{}{}", m.host_path, m.container_path, if m.read_only { ":ro" } else { "" })).collect();
        let resolv = self.sandbox_dir(&sandbox.id).join("resolv.conf");
        if resolv.exists() && !cfg.mounts.iter().any(|m| m.container_path == "/etc/resolv.conf") {
            binds.push(format!("{}:/etc/resolv.conf", resolv.display()));
        }
        let mut host = json!({
            "NetworkMode": format!("container:{}", sandbox.id),
            "IpcMode": format!("container:{}", sandbox.id),
            "Binds": binds,
            "RestartPolicy": {"Name": "no"},
            "Privileged": cfg.privileged,
            "ReadonlyRootfs": cfg.read_only_rootfs,
            "CapAdd": cfg.cap_add,
            "CapDrop": cfg.cap_drop,
        });
        if let Some(m) = cfg.cpu_limit_milli.filter(|m| *m > 0) {
            host["NanoCpus"] = json!(m * 1_000_000);
        }
        if let Some(m) = cfg.memory_limit.filter(|m| *m > 0) {
            host["Memory"] = json!(m);
        }
        let mut body = json!({
            "Image": cfg.image,
            "Env": cfg.env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>(),
            "Labels": lbl,
            "Tty": cfg.tty,
            "OpenStdin": cfg.stdin,
            "StdinOnce": cfg.stdin_once,
            "User": user,
            "HostConfig": host,
        });
        if !cfg.command.is_empty() {
            body["Entrypoint"] = json!(cfg.command);
            // An explicit entrypoint resets the image CMD (like `docker run --entrypoint`).
            body["Cmd"] = json!(cfg.args);
        } else if !cfg.args.is_empty() {
            body["Cmd"] = json!(cfg.args);
        }
        if let Some(wd) = &cfg.working_dir {
            body["WorkingDir"] = json!(wd);
        }
        let name = format!("k8s_{}_{}_{}_{}_{}", cfg.name, pod_name, pod_ns, sandbox.pod_uid, cfg.attempt);
        let r = self.client.call(Method::POST, &format!("/containers/create?name={}", enc(&name)), Some(&body)).await?;
        r["Id"].as_str().map(String::from).ok_or_else(|| anyhow!("no container id"))
    }

    async fn start_container(&self, id: &str) -> Result<()> {
        self.client.call(Method::POST, &format!("/containers/{id}/start"), None).await.map(|_| ())
    }

    async fn stop_container(&self, id: &str, timeout_secs: i64) -> Result<()> {
        match self.client.call(Method::POST, &format!("/containers/{id}/stop?t={}", timeout_secs.max(0)), None).await {
            Ok(_) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) if e.to_string().contains("304") => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn remove_container(&self, id: &str) -> Result<()> {
        match self.client.call(Method::DELETE, &format!("/containers/{id}?force=1&v=1"), None).await {
            Ok(_) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) if e.to_string().contains("already in progress") => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn list_containers(&self) -> Result<Vec<ContainerStatus>> {
        let mut out = vec![];
        for id in self.list_ids(labels::TYPE_CONTAINER).await? {
            let Ok(c) = self.inspect(&id).await else { continue };
            let lbl = str_map(&c["Config"]["Labels"]);
            let st = &c["State"];
            let state = match st["Status"].as_str().unwrap_or("") {
                "created" => ContainerState::Created,
                "running" | "paused" | "restarting" => ContainerState::Running,
                "exited" | "dead" => ContainerState::Exited,
                _ => ContainerState::Unknown,
            };
            let exit_code = st["ExitCode"].as_i64().unwrap_or(0) as i32;
            let reason = match state {
                ContainerState::Exited if st["OOMKilled"] == true => Some("OOMKilled".to_string()),
                ContainerState::Exited if exit_code == 0 => Some("Completed".to_string()),
                ContainerState::Exited => Some("Error".to_string()),
                _ => None,
            };
            out.push(ContainerStatus {
                id,
                pod_uid: lbl.get(labels::POD_UID).cloned().unwrap_or_default(),
                name: lbl.get(labels::CONTAINER_NAME).cloned().unwrap_or_default(),
                attempt: lbl.get(labels::ATTEMPT).and_then(|a| a.parse().ok()).unwrap_or(0),
                state,
                created_at: time(&c["Created"]).unwrap_or_default(),
                started_at: time(&st["StartedAt"]),
                finished_at: time(&st["FinishedAt"]),
                exit_code,
                reason,
                message: st["Error"].as_str().filter(|s| !s.is_empty()).map(String::from),
                image: c["Config"]["Image"].as_str().unwrap_or("").to_string(),
                image_ref: c["Image"].as_str().unwrap_or("").to_string(),
                labels: lbl,
            });
        }
        Ok(out)
    }

    async fn image_present(&self, image: &str) -> Result<bool> {
        match self.client.call(Method::GET, &format!("/images/{}/json", enc(image)), None).await {
            Ok(_) => Ok(true),
            Err(e) if is_not_found(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn pull_image(&self, image: &str) -> Result<()> {
        let (from, tag) = split_image(image);
        let mut path = format!("/images/create?fromImage={}", enc(&from));
        if !tag.is_empty() {
            path.push_str(&format!("&tag={}", enc(&tag)));
        }
        let mut stream = Box::pin(self.client.stream(Method::POST, &path).await?);
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?);
        }
        // The progress stream reports failures in-band.
        for line in String::from_utf8_lossy(&buf).lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if let Some(err) = v["error"].as_str().or(v["errorDetail"]["message"].as_str()) {
                    return Err(anyhow!("{err}"));
                }
            }
        }
        Ok(())
    }

    async fn list_images(&self) -> Result<Vec<(Vec<String>, i64)>> {
        let list = self.client.call(Method::GET, "/images/json", None).await?;
        Ok(list
            .as_array()
            .into_iter()
            .flatten()
            .map(|i| {
                let names = i["RepoTags"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|n| n.as_str().map(String::from))
                    .filter(|n| n != "<none>:<none>")
                    .collect();
                (names, i["Size"].as_i64().unwrap_or(0))
            })
            .collect())
    }

    async fn exec_sync(&self, id: &str, cmd: &[String], timeout_secs: u64) -> Result<(i32, Vec<u8>, Vec<u8>)> {
        let mut s = self.exec_stream(id, cmd, false, false).await?;
        let mut raw = Vec::new();
        let read = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs.max(1)), s.io.read_to_end(&mut raw)).await;
        if read.is_err() {
            return Err(anyhow!("command timed out after {timeout_secs}s"));
        }
        let (mut out, mut err) = (vec![], vec![]);
        for (stream, data) in Demuxer::default().push(&raw) {
            if stream == 2 {
                err.extend(data);
            } else {
                out.extend(data);
            }
        }
        let code = self.exec_exit_code(&s.exec_id).await?.unwrap_or(-1);
        Ok((code, out, err))
    }

    async fn exec_stream(&self, id: &str, cmd: &[String], tty: bool, stdin: bool) -> Result<ExecSession> {
        let exec_id = self.exec_create(id, cmd, tty, stdin).await?;
        let io = self.client.hijack(&format!("/exec/{exec_id}/start"), &json!({"Detach": false, "Tty": tty})).await?;
        Ok(ExecSession { exec_id, io: Box::new(io), tty })
    }

    async fn attach(&self, id: &str, stdin: bool, tty: bool) -> Result<ExecSession> {
        let path = format!("/containers/{id}/attach?stream=1&stdin={}&stdout=1&stderr=1", stdin as u8);
        let io = self.client.hijack(&path, &json!({})).await?;
        Ok(ExecSession { exec_id: id.to_string(), io: Box::new(io), tty })
    }

    /// `GET /containers/{id}/stats?stream=false&one-shot=true`: the cgroup counters
    /// (cAdvisor's working set = usage - inactive_file).
    async fn container_stats(&self, id: &str) -> Result<ContainerStats> {
        let v = self.client.call(Method::GET, &format!("/containers/{id}/stats?stream=false&one-shot=true"), None).await?;
        Ok(parse_stats(&v))
    }

    async fn resize_container(&self, id: &str, width: u16, height: u16) -> Result<()> {
        self.client.call(Method::POST, &format!("/containers/{id}/resize?h={height}&w={width}"), None).await.map(|_| ())
    }

    /// cri-dockerd runs `nsenter -t <pid> -n socat - TCP4:localhost:PORT` as root. The
    /// kubelet here is unprivileged, so the same copy runs in a short-lived container
    /// that joins the sandbox's network namespace: the pause image (no libc) with the
    /// static `rustkube-portforward` helper mounted as its entrypoint, attached to
    /// the stream through its stdin/stdout.
    async fn port_forward(&self, sandbox_id: &str, port: u16, stream: tokio::io::DuplexStream) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let helper = self
            .portforward_helper
            .as_ref()
            .ok_or_else(|| anyhow!("port forwarding is unavailable: the rustkube-portforward helper was not found next to the kubelet"))?;
        self.ensure_image(&self.pause_image).await?;
        let body = json!({
            "Image": self.pause_image,
            "Entrypoint": ["/rustkube-portforward"],
            "Cmd": [port.to_string()],
            "OpenStdin": true,
            "StdinOnce": true,
            "AttachStdin": true,
            "AttachStdout": true,
            "AttachStderr": true,
            "Labels": {labels::NODE: self.node, "io.rustkubernetes.portforward": sandbox_id},
            "HostConfig": {
                "NetworkMode": format!("container:{sandbox_id}"),
                "Binds": [format!("{}:/rustkube-portforward:ro", helper.display())],
                "RestartPolicy": {"Name": "no"},
            },
        });
        let r = self.client.call(Method::POST, "/containers/create", Some(&body)).await?;
        let id = r["Id"].as_str().ok_or_else(|| anyhow!("no port-forward container id"))?.to_string();
        let result = async {
            let io = self.client.hijack(&format!("/containers/{id}/attach?stream=1&stdin=1&stdout=1&stderr=1"), &json!({})).await?;
            self.client.call(Method::POST, &format!("/containers/{id}/start"), None).await?;
            let (mut c_rd, mut c_wr) = tokio::io::split(io);
            let (mut s_rd, mut s_wr) = tokio::io::split(stream);
            // Client → pod; at EOF close the helper's stdin (it half-closes the socket).
            let input = tokio::spawn(async move {
                let _ = tokio::io::copy(&mut s_rd, &mut c_wr).await;
                let _ = c_wr.shutdown().await;
            });
            // Pod → client (stdout); stderr carries the helper's error message.
            let mut demux = Demuxer::default();
            let mut stderr = Vec::new();
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                match c_rd.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        for (st, data) in demux.push(&buf[..n]) {
                            if st == 2 {
                                stderr.extend_from_slice(&data);
                            } else if s_wr.write_all(&data).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }
            let _ = s_wr.shutdown().await;
            input.abort();
            let w = self.client.call(Method::POST, &format!("/containers/{id}/wait"), None).await?;
            match w["StatusCode"].as_i64().unwrap_or(0) {
                0 => Ok(()),
                code => {
                    let msg = String::from_utf8_lossy(&stderr).trim().to_string();
                    Err(anyhow!("exit status {code}: {msg}"))
                }
            }
        }
        .await;
        let _ = self.client.call(Method::DELETE, &format!("/containers/{id}?force=1"), None).await;
        result
    }

    async fn exec_resize(&self, exec_id: &str, width: u16, height: u16) -> Result<()> {
        self.client.call(Method::POST, &format!("/exec/{exec_id}/resize?h={height}&w={width}"), None).await.map(|_| ())
    }

    async fn exec_exit_code(&self, exec_id: &str) -> Result<Option<i32>> {
        let v = self.client.call(Method::GET, &format!("/exec/{exec_id}/json"), None).await?;
        Ok(if v["Running"] == true { None } else { v["ExitCode"].as_i64().map(|c| c as i32) })
    }

    async fn logs(&self, id: &str, opts: &LogOptions) -> Result<LogStream> {
        let c = self.inspect(id).await?;
        let tty = c["Config"]["Tty"] == true;
        let mut path = format!("/containers/{id}/logs?stdout=1&stderr=1&follow={}&timestamps={}", opts.follow as u8, opts.timestamps as u8);
        if let Some(t) = opts.tail_lines {
            path.push_str(&format!("&tail={t}"));
        }
        if let Some(s) = opts.since_seconds {
            path.push_str(&format!("&since={}", chrono::Utc::now().timestamp() - s));
        }
        let stream = self.client.stream(Method::GET, &path).await?;
        let mut demux = Demuxer::default();
        let s = stream.flat_map(move |chunk| {
            let frames: Vec<Result<(u8, bytes::Bytes)>> = match chunk {
                Err(e) => vec![Err(e)],
                Ok(b) if tty => vec![Ok((1, b))],
                Ok(b) => demux.push(&b).into_iter().map(|(s, d)| Ok((s, bytes::Bytes::from(d)))).collect(),
            };
            futures::stream::iter(frames)
        });
        Ok(Box::pin(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_names() {
        assert_eq!(split_image("nginx"), ("nginx".into(), "latest".into()));
        assert_eq!(split_image("nginx:1.27"), ("nginx".into(), "1.27".into()));
        assert_eq!(split_image("localhost:5000/app"), ("localhost:5000/app".into(), "latest".into()));
        assert_eq!(split_image("localhost:5000/app:v1"), ("localhost:5000/app".into(), "v1".into()));
        assert_eq!(split_image("registry.k8s.io/pause:3.10"), ("registry.k8s.io/pause".into(), "3.10".into()));
        assert_eq!(split_image("x@sha256:ab"), ("x@sha256:ab".into(), "".into()));
    }
}

#[cfg(test)]
mod docker_tests {
    use super::*;

    /// Needs Docker: `DOCKER_TESTS=1 cargo test -p kube-kubelet host_path_through_docker`.
    #[tokio::test]
    async fn host_path_through_docker() {
        if std::env::var("DOCKER_TESTS").is_err() {
            return;
        }
        let base = std::env::temp_dir().join(format!("rk-hostpath-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o555)).unwrap();
        let rt = DockerRuntime::new("unix:///var/run/docker.sock", "test-node", "registry.k8s.io/pause:3.10");
        rt.ensure_host_path(&base.join("xtables.lock"), false).await.unwrap();
        rt.ensure_host_path(&base.join("flannel"), true).await.unwrap();
        assert!(base.join("xtables.lock").is_file());
        assert!(base.join("flannel").is_dir());
        let _ = tokio::process::Command::new("docker")
            .args(["run", "--rm", "-v", &format!("{}:/x", base.display()), "busybox:1.36", "rm", "-rf", "/x/xtables.lock", "/x/flannel"])
            .status()
            .await;
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }
}
