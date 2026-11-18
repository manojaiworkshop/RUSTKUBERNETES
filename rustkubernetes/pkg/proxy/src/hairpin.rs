//! Hairpin mode for pod bridge ports (the job of kubelet's `--hairpin-mode=hairpin-veth`,
//! `pkg/kubelet/network/hairpin`).
//!
//! When a pod reaches its own Service and the load balancer picks the pod itself,
//! the DNATed packet must leave through the bridge port it arrived on; a Linux
//! bridge only does that with `hairpin_mode` on. The unprivileged kubelet cannot
//! set it, so kube-proxy (which has CAP_NET_ADMIN on the node) does, via netlink
//! (`bridge link set dev <port> hairpin on`).

use std::path::Path;

/// Enables hairpin mode on every port of `bridge` that does not have it yet.
pub async fn ensure(bridge: &str, bridge_cmd: &str) {
    let dir = Path::new("/sys/class/net").join(bridge).join("brif");
    let Ok(ports) = std::fs::read_dir(&dir) else {
        tracing::warn!("hairpin: bridge {bridge} not found");
        return;
    };
    for p in ports.flatten() {
        let on = std::fs::read_to_string(p.path().join("hairpin_mode")).map(|s| s.trim() == "1").unwrap_or(true);
        if on {
            continue;
        }
        let port = p.file_name().to_string_lossy().to_string();
        match tokio::process::Command::new(bridge_cmd).args(["link", "set", "dev", &port, "hairpin", "on"]).output().await {
            Ok(o) if o.status.success() => tracing::info!(bridge, port, "enabled hairpin mode"),
            Ok(o) => tracing::warn!(port, "hairpin: {}", String::from_utf8_lossy(&o.stderr).trim()),
            Err(e) => tracing::warn!("hairpin: running {bridge_cmd}: {e}"),
        }
    }
}
