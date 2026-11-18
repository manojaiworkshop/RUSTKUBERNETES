//! Clearing stale UDP conntrack entries (`pkg/proxy/conntrack`). UDP has no
//! connection teardown, so entries pointing at removed endpoints (or caching the
//! "no endpoints" reject) would otherwise black-hole clients until they expire.
//! Uses the `conntrack` tool when available; failures are logged and ignored.

use std::net::Ipv4Addr;

pub struct Conntrack {
    path: Option<String>,
}

impl Conntrack {
    /// Looks for the tool; `None` disables cleanup.
    pub fn detect(path: &str) -> Self {
        let found = std::process::Command::new(path)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !found {
            tracing::warn!("{path} not found: stale UDP conntrack entries will not be cleared");
        }
        Self { path: found.then(|| path.to_string()) }
    }

    async fn delete(&self, args: &[String]) {
        let Some(path) = &self.path else { return };
        let mut a = vec!["-D".to_string(), "-p".to_string(), "udp".to_string()];
        a.extend_from_slice(args);
        match tokio::process::Command::new(path).args(&a).output().await {
            // Exit status 1 means "0 flow entries deleted".
            Ok(o) if o.status.success() || o.status.code() == Some(1) => tracing::debug!(args = ?a, "conntrack entries cleared"),
            Ok(o) => tracing::warn!(args = ?a, "conntrack failed: {}", String::from_utf8_lossy(&o.stderr).trim()),
            Err(e) => tracing::warn!("conntrack: {e}"),
        }
    }

    /// `ClearEntriesForIP`-style: flows to a service IP:port (e.g. cached rejects).
    pub async fn clear_service(&self, ip: Ipv4Addr, port: u16) {
        self.delete(&["--orig-dst".into(), ip.to_string(), "--orig-port-dst".into(), port.to_string()]).await;
    }

    /// Flows to a NodePort.
    pub async fn clear_node_port(&self, port: u16) {
        self.delete(&["--orig-port-dst".into(), port.to_string()]).await;
    }

    /// `ClearEntriesForNAT`: flows to `origin` that were DNATed to a removed endpoint.
    pub async fn clear_nat(&self, origin: Ipv4Addr, endpoint: Ipv4Addr) {
        self.delete(&["--orig-dst".into(), origin.to_string(), "--dst-nat".into(), endpoint.to_string()]).await;
    }
}
