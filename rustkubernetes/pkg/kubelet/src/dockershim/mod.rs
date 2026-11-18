//! Docker implementation of [`Runtime`] (the approach of upstream's former
//! `dockershim` and today's `cri-dockerd`).
//!
//! * sandbox = a `pause` container that owns the pod's network namespace
//!   (bridge network, or the host network for `hostNetwork` pods);
//! * app containers use `NetworkMode/IpcMode = container:<sandbox>` so all
//!   containers of a pod share localhost and the pod IP;
//! * names follow dockershim: `k8s_<container>_<pod>_<namespace>_<uid>_<attempt>`.

pub mod client;

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
        Self { client: DockerClient::new(endpoint), node: node.into(), pause_image: pause_image.into() }
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

    async fn run_pod_sandbox(&self, cfg: &SandboxConfig) -> Result<String> {
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
                "NetworkMode": if cfg.host_network { "host" } else { "bridge" },
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
        self.remove_container(id).await
    }

    async fn list_pod_sandboxes(&self) -> Result<Vec<SandboxStatus>> {
        let mut out = vec![];
        for id in self.list_ids(labels::TYPE_SANDBOX).await? {
            let Ok(c) = self.inspect(&id).await else { continue };
            let lbl = str_map(&c["Config"]["Labels"]);
            let ip = c["NetworkSettings"]["Networks"]["bridge"]["IPAddress"]
                .as_str()
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
        let mut host = json!({
            "NetworkMode": format!("container:{}", sandbox.id),
            "IpcMode": format!("container:{}", sandbox.id),
            "Binds": cfg.mounts.iter().map(|m| format!("{}:{}{}", m.host_path, m.container_path, if m.read_only { ":ro" } else { "" })).collect::<Vec<_>>(),
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
