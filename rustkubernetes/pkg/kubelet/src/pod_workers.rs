//! Per-pod workers (`pod_workers.go`) and the runtime sync
//! (`kuberuntime_manager.go: SyncPod`, `KillPod`).
//!
//! Every pod bound to this node gets one worker task. Each sync:
//! 1. terminating pod → stop containers (preStop, grace period), remove the
//!    sandbox, clean volumes, confirm deletion with `gracePeriodSeconds=0`;
//! 2. set up volumes;
//! 3. ensure a ready sandbox (recreate it — and its containers — if it died);
//! 4. run init containers one at a time;
//! 5. start/restart app containers (restart policy, CrashLoopBackOff, spec changes);
//! 6. run probes (startup → liveness/readiness);
//! 7. write the pod status if it changed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use k8s_api::core::v1::{Container, Pod};
use k8s_client_go::tools::record::{object_ref, NORMAL, WARNING};
use k8s_client_go::{Api, DeleteParams};
use serde_json::Value;
use tokio::sync::watch;

use crate::prober::{self, ProbeKind, ProbeTracker};
use crate::runtime::{labels, ContainerConfig, ContainerState, ContainerStatus, PortMapping, SandboxConfig, SandboxState, SandboxStatus};
use crate::status::{self, attempts, ContainerExtra};
use crate::{envvars, volumes, KubeletCtx};

const MAX_BACKOFF: Duration = Duration::from_secs(300);
const INITIAL_BACKOFF: Duration = Duration::from_secs(10);

pub enum Outcome {
    Continue,
    Finished,
}

pub struct PodWorker {
    ctx: Arc<KubeletCtx>,
    uid: String,
    /// container name → (restarts, not before)
    backoff: HashMap<String, (u32, Instant)>,
    extras: HashMap<String, ContainerExtra>,
    /// Containers being stopped after a failed liveness/startup probe.
    killing: std::collections::HashSet<String>,
    probes: HashMap<(String, ProbeKind), ProbeTracker>,
    /// runtime container id → when we saw it start
    started_at: HashMap<String, Instant>,
    last_event: HashMap<String, String>,
}

/// FNV-1a hash of the container spec (`HashContainer`): a change restarts it.
fn container_hash(c: &Container) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in serde_json::to_string(c).unwrap_or_default().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:x}")
}

fn quantity_milli(v: Option<&String>) -> Option<i64> {
    v.and_then(|q| k8s_apimachinery::quantity::parse_milli(q).ok()).map(|m| m as i64)
}

impl PodWorker {
    pub fn new(ctx: Arc<KubeletCtx>, uid: String) -> Self {
        Self {
            ctx,
            uid,
            backoff: HashMap::new(),
            extras: HashMap::new(),
            killing: Default::default(),
            probes: HashMap::new(),
            started_at: HashMap::new(),
            last_event: HashMap::new(),
        }
    }

    /// Records an event about the pod (or one of its containers), skipping exact repeats.
    fn event(&mut self, pod: &Pod, container: Option<&str>, type_: &str, reason: &str, msg: String) {
        let key = format!("{}/{reason}", container.unwrap_or(""));
        if self.last_event.get(&key) == Some(&msg) {
            return;
        }
        self.last_event.insert(key, msg.clone());
        let mut r = object_ref(pod);
        r.field_path = container.map(|c| format!("spec.containers{{{c}}}"));
        self.ctx.recorder.event_ref(r, type_, reason, msg);
    }

    fn extra(&mut self, name: &str) -> &mut ContainerExtra {
        self.extras.entry(name.to_string()).or_default()
    }

    /// Worker loop: syncs on every pod update and at least once per second.
    pub async fn run(mut self, mut rx: watch::Receiver<Option<Arc<Pod>>>) {
        loop {
            let pod = rx.borrow_and_update().clone();
            let outcome = match pod {
                Some(p) => self.sync(&p).await,
                None => {
                    self.kill(None, 0).await;
                    volumes::teardown(&self.ctx.root, &self.uid);
                    Outcome::Finished
                }
            };
            if let Outcome::Finished = outcome {
                break;
            }
            tokio::select! {
                r = rx.changed() => if r.is_err() { break },
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
        self.ctx.workers.lock().unwrap().remove(&self.uid);
    }

    async fn sync(&mut self, pod: &Pod) -> Outcome {
        let ns = pod.metadata.namespace().to_string();
        let name = pod.metadata.name().to_string();
        if pod.metadata.deletion_timestamp.is_some() {
            let grace = pod.metadata.deletion_grace_period_seconds.unwrap_or(30);
            self.kill(Some(pod), grace).await;
            volumes::teardown(&self.ctx.root, &self.uid);
            let api: Api<Pod> = Api::namespaced(self.ctx.client.clone(), &ns);
            let dp = DeleteParams { grace_period_seconds: Some(0), precondition_uid: pod.metadata.uid.clone(), ..Default::default() };
            match api.delete_with(&name, &dp).await {
                Ok(_) => tracing::info!(pod = %format!("{ns}/{name}"), "pod terminated and deleted"),
                Err(e) if e.is_not_found() || e.is_conflict() => {}
                Err(e) => {
                    tracing::warn!(pod = %format!("{ns}/{name}"), "failed to confirm deletion: {e}");
                    return Outcome::Continue;
                }
            }
            return Outcome::Finished;
        }
        let phase = pod.status.as_ref().and_then(|s| s.phase.as_deref()).unwrap_or("Pending");
        if phase == "Succeeded" || phase == "Failed" {
            // Terminal: make sure nothing runs anymore, keep containers for `kubectl logs`.
            let (sandboxes, containers) = self.ctx.cache.for_pod(&self.uid);
            for c in containers.iter().filter(|c| c.state == ContainerState::Running) {
                let _ = self.ctx.runtime.stop_container(&c.id, 0).await;
            }
            for s in sandboxes.iter().filter(|s| s.state == SandboxState::Ready) {
                let _ = self.ctx.runtime.stop_pod_sandbox(&s.id).await;
            }
            return Outcome::Continue;
        }
        if let Err(e) = self.sync_running(pod).await {
            tracing::warn!(pod = %format!("{ns}/{name}"), "sync failed: {e:#}");
        }
        self.ctx.cache.refresh(self.ctx.runtime.as_ref()).await;
        self.post_status(pod).await;
        Outcome::Continue
    }

    async fn sync_running(&mut self, pod: &Pod) -> anyhow::Result<()> {
        let spec = pod.spec.clone().unwrap_or_default();
        // If the network plugin is not ready, only start the pod if it uses the host network.
        if let Some(e) = self.ctx.runtime_state.network_error().filter(|_| !spec.host_network.unwrap_or(false)) {
            let msg = format!("network is not ready: {e}");
            self.event(pod, None, WARNING, "NetworkNotReady", msg.clone());
            for c in spec.containers.iter().chain(spec.init_containers.iter().flatten()) {
                self.extra(&c.name).waiting = Some(("ContainerCreating".into(), String::new()));
            }
            anyhow::bail!(msg);
        }
        // Volumes.
        let vols = match volumes::setup(&self.ctx.client, self.ctx.runtime.as_ref(), &self.ctx.root, pod).await {
            Ok(v) => {
                // Clear markers left by an earlier (transient) mount failure.
                for e in self.extras.values_mut() {
                    if e.waiting.as_ref().is_some_and(|(r, _)| r == "ContainerCreating") {
                        e.waiting = None;
                    }
                }
                v
            }
            Err(e) => {
                self.event(pod, None, WARNING, "FailedMount", format!("MountVolume.SetUp failed: {e}"));
                for c in spec.containers.iter().chain(spec.init_containers.iter().flatten()) {
                    self.extra(&c.name).waiting = Some(("ContainerCreating".into(), String::new()));
                }
                return Ok(());
            }
        };
        // Sandbox.
        let (sandboxes, _) = self.ctx.cache.for_pod(&self.uid);
        let latest = sandboxes.iter().max_by_key(|s| s.attempt).cloned();
        let sandbox = match latest {
            Some(s) if s.state == SandboxState::Ready => s,
            other => {
                if other.is_some() {
                    self.event(pod, None, NORMAL, "SandboxChanged", "Pod sandbox changed, it will be killed and re-created.".into());
                    self.kill(None, 0).await;
                }
                let attempt = other.map(|s| s.attempt + 1).unwrap_or(0);
                let cfg = self.sandbox_config(pod, attempt);
                if let Err(e) = self.ctx.runtime.run_pod_sandbox(&cfg).await {
                    self.event(pod, None, WARNING, "FailedCreatePodSandBox", format!("Failed to create pod sandbox: {e}"));
                    return Ok(());
                }
                self.ctx.cache.refresh(self.ctx.runtime.as_ref()).await;
                let (sb, _) = self.ctx.cache.for_pod(&self.uid);
                match sb.into_iter().filter(|s| s.state == SandboxState::Ready).max_by_key(|s| s.attempt) {
                    Some(s) => s,
                    None => return Ok(()),
                }
            }
        };
        let (_, containers) = self.ctx.cache.for_pod(&self.uid);
        let restart = spec.restart_policy.clone().unwrap_or_else(|| "Always".into());
        self.killing.retain(|id| containers.iter().any(|c| &c.id == id && c.state == ContainerState::Running));

        // Init containers, one at a time.
        for c in spec.init_containers.iter().flatten() {
            let att = attempts(&containers, &c.name);
            match att.first() {
                None => {
                    self.start(pod, &sandbox, c, 0, &vols).await;
                    return Ok(());
                }
                Some(l) if l.state == ContainerState::Running => return Ok(()),
                Some(l) if l.state == ContainerState::Exited && l.exit_code == 0 => continue,
                Some(l) => {
                    if restart == "Never" && l.state == ContainerState::Exited {
                        return Ok(()); // pod fails
                    }
                    let attempt = l.attempt + 1;
                    if self.backoff_ok(pod, &c.name) {
                        self.start(pod, &sandbox, c, attempt, &vols).await;
                    }
                    return Ok(());
                }
            }
        }

        // App containers.
        for c in &spec.containers {
            let att = attempts(&containers, &c.name);
            let hash = container_hash(c);
            match att.first().copied() {
                None => self.start(pod, &sandbox, c, 0, &vols).await,
                Some(l) if l.state == ContainerState::Running => {
                    if l.labels.get(labels::CONTAINER_HASH) != Some(&hash) {
                        self.event(
                            pod,
                            Some(&c.name),
                            NORMAL,
                            "Killing",
                            format!("Container {} definition changed, will be restarted", c.name),
                        );
                        self.ctx.runtime.stop_container(&l.id, spec.termination_grace_period_seconds.unwrap_or(30)).await?;
                        continue;
                    }
                    self.probe(pod, &sandbox, c, l).await;
                }
                Some(l) if l.state == ContainerState::Created || l.state == ContainerState::Unknown => {
                    self.ctx.runtime.remove_container(&l.id).await?;
                    self.start(pod, &sandbox, c, l.attempt + 1, &vols).await;
                }
                Some(l) => {
                    let should = match restart.as_str() {
                        "Never" => false,
                        "OnFailure" => l.exit_code != 0,
                        _ => true,
                    };
                    if !should {
                        continue;
                    }
                    if self.backoff_ok(pod, &c.name) {
                        // Keep only the previous attempt (for `kubectl logs --previous`).
                        for old in att.iter().skip(1) {
                            let _ = self.ctx.runtime.remove_container(&old.id).await;
                        }
                        self.start(pod, &sandbox, c, l.attempt + 1, &vols).await;
                    }
                }
            }
        }
        Ok(())
    }

    /// Returns true if the container may be (re)started now; otherwise reports CrashLoopBackOff.
    fn backoff_ok(&mut self, pod: &Pod, name: &str) -> bool {
        let now = Instant::now();
        let entry = self.backoff.get(name).copied();
        match entry {
            Some((n, until)) if now < until => {
                let secs = (INITIAL_BACKOFF * 2u32.pow(n.saturating_sub(1).min(6))).min(MAX_BACKOFF).as_secs();
                let msg =
                    format!("back-off {secs}s restarting failed container={name} pod={}_{}", pod.metadata.name(), pod.metadata.namespace());
                self.extra(name).waiting = Some(("CrashLoopBackOff".into(), msg.clone()));
                self.event(
                    pod,
                    Some(name),
                    WARNING,
                    "BackOff",
                    format!("Back-off restarting failed container {name} in pod {}", pod.metadata.name()),
                );
                false
            }
            _ => {
                let n = entry.map(|(n, _)| n + 1).unwrap_or(1);
                let delay = (INITIAL_BACKOFF * 2u32.pow(n.saturating_sub(1).min(6))).min(MAX_BACKOFF);
                self.backoff.insert(name.to_string(), (n, now + delay));
                true
            }
        }
    }

    fn sandbox_config(&self, pod: &Pod, attempt: u32) -> SandboxConfig {
        let spec = pod.spec.clone().unwrap_or_default();
        let ports = spec
            .containers
            .iter()
            .flat_map(|c| c.ports.iter().flatten())
            .filter(|p| p.host_port.unwrap_or(0) > 0)
            .map(|p| PortMapping {
                container_port: p.container_port,
                host_port: p.host_port.unwrap_or(0),
                protocol: p.protocol.clone().unwrap_or_else(|| "TCP".into()),
                host_ip: p.host_ip.clone().unwrap_or_default(),
            })
            .collect();
        SandboxConfig {
            pod_uid: self.uid.clone(),
            name: pod.metadata.name().into(),
            namespace: pod.metadata.namespace().into(),
            attempt,
            hostname: spec.hostname.clone().unwrap_or_else(|| pod.metadata.name().chars().take(63).collect()),
            host_network: spec.host_network.unwrap_or(false),
            port_mappings: ports,
            labels: Default::default(),
            dns: crate::dns::pod_dns(pod, &self.ctx.cluster_dns, &self.ctx.cluster_domain),
        }
    }

    /// Pull (per policy), create and start one container.
    async fn start(&mut self, pod: &Pod, sandbox: &SandboxStatus, c: &Container, attempt: u32, vols: &HashMap<String, std::path::PathBuf>) {
        let image = c.image.clone().unwrap_or_default();
        let rt = self.ctx.runtime.clone();
        // Image.
        let policy = c.image_pull_policy.clone().unwrap_or_else(|| "IfNotPresent".into());
        let present = rt.image_present(&image).await.unwrap_or(false);
        if policy == "Never" && !present {
            let msg = format!("Container image \"{image}\" is not present with pull policy of Never");
            self.extra(&c.name).waiting = Some(("ErrImageNeverPull".into(), msg.clone()));
            self.event(pod, Some(&c.name), WARNING, "ErrImageNeverPull", msg);
            return;
        }
        if policy == "Always" || !present {
            let key = format!("image:{}", c.name);
            if let Some((_, until)) = self.backoff.get(&key) {
                if Instant::now() < *until {
                    self.extra(&c.name).waiting = Some(("ImagePullBackOff".into(), format!("Back-off pulling image \"{image}\"")));
                    return;
                }
            }
            self.event(pod, Some(&c.name), NORMAL, "Pulling", format!("Pulling image \"{image}\""));
            let t0 = Instant::now();
            if let Err(e) = rt.pull_image(&image).await {
                let n = self.backoff.get(&key).map(|(n, _)| n + 1).unwrap_or(1);
                self.backoff.insert(key, (n, Instant::now() + (INITIAL_BACKOFF * 2u32.pow((n - 1).min(5))).min(MAX_BACKOFF)));
                let msg = format!("Failed to pull image \"{image}\": {e}");
                self.extra(&c.name).waiting = Some(("ErrImagePull".into(), msg.clone()));
                self.event(pod, Some(&c.name), WARNING, "Failed", msg);
                return;
            }
            self.backoff.remove(&key);
            self.event(
                pod,
                Some(&c.name),
                NORMAL,
                "Pulled",
                format!("Successfully pulled image \"{image}\" in {:.3}s", t0.elapsed().as_secs_f64()),
            );
        }
        // Environment, mounts, command.
        let env = match envvars::build(&self.ctx.client, pod, c).await {
            Ok(e) => e,
            Err(e) => {
                self.extra(&c.name).waiting = Some(("CreateContainerConfigError".into(), e.to_string()));
                self.event(pod, Some(&c.name), WARNING, "Failed", format!("Error: {e}"));
                return;
            }
        };
        let mounts = match volumes::mounts_for(c, vols) {
            Ok(m) => m,
            Err(e) => {
                self.extra(&c.name).waiting = Some(("CreateContainerConfigError".into(), e.to_string()));
                return;
            }
        };
        let env_map = env.iter().cloned().collect();
        let expand =
            |v: &Option<Vec<String>>| v.clone().unwrap_or_default().iter().map(|s| envvars::expand(s, &env_map)).collect::<Vec<_>>();
        let pod_sc = pod.spec.as_ref().and_then(|s| s.security_context.clone()).unwrap_or(Value::Null);
        let sc = c.security_context.clone().unwrap_or(Value::Null);
        let pick = |k: &str| sc.get(k).and_then(Value::as_i64).or_else(|| pod_sc.get(k).and_then(Value::as_i64));
        let caps = |k: &str| {
            sc.pointer(&format!("/capabilities/{k}"))
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default()
        };
        let limits = c.resources.as_ref().and_then(|r| r.limits.clone()).unwrap_or_default();
        let mut lbl = std::collections::BTreeMap::new();
        lbl.insert(labels::CONTAINER_HASH.to_string(), container_hash(c));
        let cfg = ContainerConfig {
            name: c.name.clone(),
            attempt,
            image: image.clone(),
            command: expand(&c.command),
            args: expand(&c.args),
            working_dir: c.working_dir.clone(),
            env,
            mounts,
            labels: lbl,
            tty: c.tty.unwrap_or(false),
            stdin: c.stdin.unwrap_or(false),
            stdin_once: c.stdin_once.unwrap_or(false),
            cpu_limit_milli: quantity_milli(limits.get("cpu")),
            memory_limit: quantity_milli(limits.get("memory")).map(|m| m / 1000),
            run_as_user: pick("runAsUser"),
            run_as_group: pick("runAsGroup"),
            privileged: sc.get("privileged") == Some(&Value::Bool(true)),
            read_only_rootfs: sc.get("readOnlyRootFilesystem") == Some(&Value::Bool(true)),
            cap_add: caps("add"),
            cap_drop: caps("drop"),
        };
        let id = match rt.create_container(sandbox, &cfg).await {
            Ok(id) => id,
            Err(e) => {
                self.extra(&c.name).waiting = Some(("CreateContainerError".into(), e.to_string()));
                self.event(pod, Some(&c.name), WARNING, "Failed", format!("Error: {e}"));
                return;
            }
        };
        self.event(pod, Some(&c.name), NORMAL, "Created", format!("Created container {}", c.name));
        if let Err(e) = rt.start_container(&id).await {
            self.extra(&c.name).waiting = Some(("RunContainerError".into(), e.to_string()));
            self.event(pod, Some(&c.name), WARNING, "Failed", format!("Error: {e}"));
            return;
        }
        self.event(pod, Some(&c.name), NORMAL, "Started", format!("Started container {}", c.name));
        self.last_event.retain(|k, _| !k.starts_with(&format!("{}/", c.name)) || k.ends_with("/Started"));
        let ex = self.extra(&c.name);
        ex.waiting = None;
        ex.started = c.startup_probe.is_none();
        ex.ready = false;
        self.started_at.insert(id.clone(), Instant::now());
        self.probes.retain(|(n, _), _| n != &c.name);
        // postStart hook (exec only).
        if let Some(cmd) = c.lifecycle.as_ref().and_then(|l| l.pointer("/postStart/exec/command")).and_then(Value::as_array) {
            let cmd: Vec<String> = cmd.iter().filter_map(|x| x.as_str().map(String::from)).collect();
            if let Ok((code, _, _)) = rt.exec_sync(&id, &cmd, 30).await {
                if code != 0 {
                    self.event(pod, Some(&c.name), WARNING, "FailedPostStartHook", format!("PostStartHook failed with exit code {code}"));
                }
            }
        }
        self.ctx.cache.refresh(rt.as_ref()).await;
    }

    /// Runs due probes for a running container.
    /// Stops a container in the background so the worker keeps reporting status
    /// while the grace period runs (`killContainer` from the probe result handler).
    fn kill_unhealthy(&mut self, pod: &Pod, c: &Container, rc: &ContainerStatus) {
        let grace = pod.spec.as_ref().and_then(|s| s.termination_grace_period_seconds).unwrap_or(30);
        let e = self.extra(&c.name);
        e.ready = false;
        self.killing.insert(rc.id.clone());
        let (rt, id) = (self.ctx.runtime.clone(), rc.id.clone());
        tokio::spawn(async move {
            let _ = rt.stop_container(&id, grace).await;
        });
    }

    async fn probe(&mut self, pod: &Pod, sandbox: &SandboxStatus, c: &Container, rc: &ContainerStatus) {
        if self.killing.contains(&rc.id) {
            return;
        }
        let started = *self.started_at.entry(rc.id.clone()).or_insert_with(Instant::now);
        let ip = if pod.spec.as_ref().and_then(|s| s.host_network).unwrap_or(false) {
            self.ctx.node_ip.clone()
        } else {
            sandbox.ip.clone().unwrap_or_default()
        };
        // Startup probe gates the others.
        if let Some(p) = &c.startup_probe {
            if !self.extra(&c.name).started {
                let tr = self.probes.entry((c.name.clone(), ProbeKind::Startup)).or_default();
                if tr.due(p, started) {
                    let r = prober::run(self.ctx.runtime.as_ref(), p, &rc.id, &ip, c).await;
                    let tr = self.probes.get_mut(&(c.name.clone(), ProbeKind::Startup)).unwrap();
                    match tr.record(p, r.is_ok(), r.err().unwrap_or_default()) {
                        Some(true) => self.extra(&c.name).started = true,
                        Some(false) => {
                            let msg = tr.last_message.clone();
                            self.event(pod, Some(&c.name), WARNING, "Unhealthy", format!("Startup probe failed: {msg}"));
                            self.event(
                                pod,
                                Some(&c.name),
                                NORMAL,
                                "Killing",
                                format!("Container {} failed startup probe, will be restarted", c.name),
                            );
                            self.kill_unhealthy(pod, c, rc);
                        }
                        None => {}
                    }
                }
                return;
            }
        }
        self.extra(&c.name).started = true;
        if let Some(p) = &c.liveness_probe {
            let due = self.probes.entry((c.name.clone(), ProbeKind::Liveness)).or_default().due(p, started);
            if due {
                let r = prober::run(self.ctx.runtime.as_ref(), p, &rc.id, &ip, c).await;
                let tr = self.probes.get_mut(&(c.name.clone(), ProbeKind::Liveness)).unwrap();
                if tr.record(p, r.is_ok(), r.err().unwrap_or_default()) == Some(false) {
                    let msg = tr.last_message.clone();
                    self.probes.remove(&(c.name.clone(), ProbeKind::Liveness));
                    self.event(pod, Some(&c.name), WARNING, "Unhealthy", format!("Liveness probe failed: {msg}"));
                    self.event(
                        pod,
                        Some(&c.name),
                        NORMAL,
                        "Killing",
                        format!("Container {} failed liveness probe, will be restarted", c.name),
                    );
                    self.kill_unhealthy(pod, c, rc);
                    return;
                }
            }
        }
        match &c.readiness_probe {
            None => self.extra(&c.name).ready = true,
            Some(p) => {
                let due = self.probes.entry((c.name.clone(), ProbeKind::Readiness)).or_default().due(p, started);
                if due {
                    let r = prober::run(self.ctx.runtime.as_ref(), p, &rc.id, &ip, c).await;
                    let tr = self.probes.get_mut(&(c.name.clone(), ProbeKind::Readiness)).unwrap();
                    let changed = tr.record(p, r.is_ok(), r.err().unwrap_or_default());
                    let msg = tr.last_message.clone();
                    if let Some(v) = changed {
                        self.extra(&c.name).ready = v;
                        if !v {
                            self.event(pod, Some(&c.name), WARNING, "Unhealthy", format!("Readiness probe failed: {msg}"));
                        }
                    }
                }
            }
        }
    }

    /// Stops and removes everything of the pod (`KillPod` + cleanup).
    async fn kill(&mut self, pod: Option<&Pod>, grace: i64) {
        self.ctx.cache.refresh(self.ctx.runtime.as_ref()).await;
        let (sandboxes, containers) = self.ctx.cache.for_pod(&self.uid);
        let rt = self.ctx.runtime.clone();
        let mut stops = vec![];
        for c in containers.iter().filter(|c| c.state == ContainerState::Running) {
            if let Some(p) = pod {
                self.event(p, Some(&c.name), NORMAL, "Killing", format!("Stopping container {}", c.name));
                // preStop hook (exec only), bounded by the grace period.
                let pre = p.spec.as_ref().and_then(|s| s.containers.iter().find(|x| x.name == c.name)).and_then(|x| x.lifecycle.clone());
                if let Some(cmd) = pre.as_ref().and_then(|l| l.pointer("/preStop/exec/command")).and_then(Value::as_array) {
                    let cmd: Vec<String> = cmd.iter().filter_map(|x| x.as_str().map(String::from)).collect();
                    let _ = rt.exec_sync(&c.id, &cmd, grace.max(1) as u64).await;
                }
            }
            let (rt, id) = (rt.clone(), c.id.clone());
            stops.push(async move { rt.stop_container(&id, grace).await });
        }
        futures::future::join_all(stops).await;
        for c in &containers {
            let _ = rt.remove_container(&c.id).await;
        }
        for s in &sandboxes {
            let _ = rt.stop_pod_sandbox(&s.id).await;
            let _ = rt.remove_pod_sandbox(&s.id).await;
        }
        self.ctx.cache.refresh(rt.as_ref()).await;
    }

    async fn post_status(&mut self, pod: &Pod) {
        let (sandboxes, containers) = self.ctx.cache.for_pod(&self.uid);
        let sandbox = sandboxes.iter().filter(|s| s.state == SandboxState::Ready).max_by_key(|s| s.attempt);
        // Waiting reasons only apply while the container is not running.
        for c in &containers {
            if c.state == ContainerState::Running {
                if let Some(e) = self.extras.get_mut(&c.name) {
                    e.waiting = None;
                }
            }
        }
        let st = status::generate(pod, sandbox, &containers, &self.extras, &self.ctx.node_ip);
        if pod.status.as_ref() == Some(&st) {
            return;
        }
        let mut p = pod.clone();
        p.status = Some(st);
        let api: Api<Pod> = Api::namespaced(self.ctx.client.clone(), pod.metadata.namespace());
        if let Err(e) = api.replace_status(&p).await {
            if !e.is_conflict() && !e.is_not_found() {
                tracing::warn!(pod = %pod.metadata.name(), "status update failed: {e}");
            }
        }
    }
}
