//! Container probes (`pkg/kubelet/prober`, `pkg/probe/{exec,http,tcp}`).
//!
//! Each (container, kind) has a [`ProbeTracker`] implementing
//! `initialDelaySeconds`, `periodSeconds`, `timeoutSeconds`,
//! `successThreshold` and `failureThreshold`.

use std::time::{Duration, Instant};

use k8s_api::core::v1::{Container, Probe};
use k8s_api::IntOrString;

use crate::runtime::Runtime;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProbeKind {
    Liveness,
    Readiness,
    Startup,
}

impl ProbeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProbeKind::Liveness => "Liveness",
            ProbeKind::Readiness => "Readiness",
            ProbeKind::Startup => "Startup",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ProbeTracker {
    last_run: Option<Instant>,
    successes: i32,
    failures: i32,
    /// Current verdict (None until the threshold is first reached).
    pub result: Option<bool>,
    pub last_message: String,
}

impl ProbeTracker {
    /// Whether the probe should run now, given when the container started.
    pub fn due(&self, probe: &Probe, started: Instant) -> bool {
        let delay = Duration::from_secs(probe.initial_delay_seconds.unwrap_or(0).max(0) as u64);
        let period = Duration::from_secs(probe.period_seconds.unwrap_or(10).max(1) as u64);
        started.elapsed() >= delay && self.last_run.is_none_or(|t| t.elapsed() >= period)
    }

    /// Records one result; returns the new verdict if it changed.
    pub fn record(&mut self, probe: &Probe, ok: bool, msg: String) -> Option<bool> {
        self.last_run = Some(Instant::now());
        self.last_message = msg;
        if ok {
            self.successes += 1;
            self.failures = 0;
        } else {
            self.failures += 1;
            self.successes = 0;
        }
        let before = self.result;
        if ok && self.successes >= probe.success_threshold.unwrap_or(1).max(1) {
            self.result = Some(true);
        } else if !ok && self.failures >= probe.failure_threshold.unwrap_or(3).max(1) {
            self.result = Some(false);
        }
        (self.result != before).then_some(self.result.unwrap_or(false))
    }
}

fn resolve_port(port: &IntOrString, c: &Container) -> Option<u16> {
    match port {
        IntOrString::Int(p) => Some(*p as u16),
        IntOrString::String(name) => {
            c.ports.iter().flatten().find(|p| p.name.as_deref() == Some(name.as_str())).map(|p| p.container_port as u16)
        }
    }
}

/// Runs one probe. `Ok(())` = success, `Err(message)` = failure.
pub async fn run(rt: &dyn Runtime, probe: &Probe, container_id: &str, pod_ip: &str, c: &Container) -> Result<(), String> {
    let timeout = Duration::from_secs(probe.timeout_seconds.unwrap_or(1).max(1) as u64);
    if let Some(exec) = &probe.exec {
        let cmd = exec.command.clone().unwrap_or_default();
        return match rt.exec_sync(container_id, &cmd, timeout.as_secs()).await {
            Ok((0, _, _)) => Ok(()),
            Ok((code, out, err)) => Err(format!(
                "command {cmd:?} exited with {code}: {}{}",
                String::from_utf8_lossy(&out).trim(),
                String::from_utf8_lossy(&err).trim()
            )),
            Err(e) => Err(e.to_string()),
        };
    }
    if let Some(h) = &probe.http_get {
        let port = resolve_port(&h.port, c).ok_or("invalid probe port")?;
        let host = h.host.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| pod_ip.to_string());
        if h.scheme.as_deref() == Some("HTTPS") {
            return Err("HTTPS probes are not supported yet".into());
        }
        let path = h.path.clone().unwrap_or_else(|| "/".into());
        let url = format!("http://{host}:{port}{}{path}", if path.starts_with('/') { "" } else { "/" });
        k8s_client_go::util::cert::install_crypto_provider();
        let client = reqwest::Client::builder().timeout(timeout).build().map_err(|e| e.to_string())?;
        return match client.get(&url).header("User-Agent", "kube-probe/1.35").send().await {
            Ok(r) if (200..400).contains(&r.status().as_u16()) => Ok(()),
            Ok(r) => Err(format!("HTTP probe failed with statuscode: {}", r.status().as_u16())),
            Err(e) => Err(format!("Get \"{url}\": {e}")),
        };
    }
    if let Some(t) = &probe.tcp_socket {
        let port = resolve_port(&t.port, c).ok_or("invalid probe port")?;
        let host = t.host.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| pod_ip.to_string());
        return match tokio::time::timeout(timeout, tokio::net::TcpStream::connect((host.as_str(), port))).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(format!("dial tcp {host}:{port}: {e}")),
            Err(_) => Err(format!("dial tcp {host}:{port}: i/o timeout")),
        };
    }
    // gRPC probes are not implemented; treat as success rather than killing pods.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds() {
        let probe = Probe { failure_threshold: Some(2), success_threshold: Some(1), ..Default::default() };
        let mut t = ProbeTracker::default();
        assert_eq!(t.record(&probe, true, String::new()), Some(true));
        assert_eq!(t.record(&probe, false, "x".into()), None); // 1 failure < 2
        assert_eq!(t.record(&probe, false, "x".into()), Some(false));
        assert_eq!(t.record(&probe, true, String::new()), Some(true));
    }
}
