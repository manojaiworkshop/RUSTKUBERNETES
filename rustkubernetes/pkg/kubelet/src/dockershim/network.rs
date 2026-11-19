//! Network plugins of the dockershim (`pkg/kubelet/dockershim/network`):
//!
//! | `--network-plugin` | Pods are attached to | NetworkReady |
//! |--------------------|----------------------|--------------|
//! | `""` (noop, default) | Docker's default `bridge` network | always |
//! | `kubenet` | the bridge `cbr0` on the node's `spec.podCIDR` | once the pod CIDR is known |
//!
//! Upstream kubenet builds `cbr0` with the CNI `bridge` and `host-local`
//! plugins. Here the same bridge is a Docker network that the kubelet creates
//! when it learns its pod CIDR (`UpdateRuntimeConfig`): subnet = pod CIDR,
//! gateway = first address on `cbr0`, the configured MTU, Docker's own
//! masquerading off and direct routing to pod IPs allowed. Pod-to-pod traffic
//! between nodes is therefore never NATed. Routes to other nodes' pod CIDRs
//! and masquerading of traffic that leaves the cluster belong to the pod
//! network add-on (flannel), as with kubenet and a route controller upstream.

use std::sync::Mutex;

use anyhow::{anyhow, Result};
use hyper::Method;
use k8s_apimachinery::net::IpNet;
use serde_json::{json, Value};

use super::client::{is_not_found, DockerClient};

pub const NOOP: &str = "";
pub const KUBENET: &str = "kubenet";
/// `kubenet.BridgeName`
pub const DEFAULT_BRIDGE: &str = "cbr0";
/// Label on the Docker network: the node that owns it.
const NODE_LABEL: &str = "io.rustkubernetes.node";

/// `--network-plugin`, `--network-plugin-mtu` and the bridge name.
#[derive(Clone, Debug, Default)]
pub struct NetworkConfig {
    pub plugin: String,
    /// 0: smallest MTU of the host's interfaces (`findMinMTU`).
    pub mtu: i32,
    pub bridge: String,
}

pub enum Plugin {
    Noop,
    Kubenet(Kubenet),
}

pub struct Kubenet {
    bridge: String,
    mtu: i32,
    node: String,
    /// The pod CIDR the bridge was built for (`netConfig`).
    net_config: Mutex<Option<IpNet>>,
}

impl Plugin {
    /// `InitNetworkPlugin`
    pub fn new(cfg: &NetworkConfig, node: &str) -> Result<Self> {
        match cfg.plugin.as_str() {
            NOOP => Ok(Plugin::Noop),
            KUBENET => {
                let mtu = if cfg.mtu > 0 { cfg.mtu } else { find_min_mtu().unwrap_or(1460) };
                let bridge = if cfg.bridge.is_empty() { DEFAULT_BRIDGE.to_string() } else { cfg.bridge.clone() };
                tracing::info!(bridge = %bridge, mtu, "kubenet network plugin initialized");
                Ok(Plugin::Kubenet(Kubenet { bridge, mtu, node: node.into(), net_config: Mutex::new(None) }))
            }
            "cni" => Err(anyhow!("network plugin \"cni\" is not supported by this runtime (use \"kubenet\")")),
            other => Err(anyhow!("network plugin {other:?} not found")),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Plugin::Noop => "noop",
            Plugin::Kubenet(_) => KUBENET,
        }
    }

    /// Docker `NetworkMode` of pod sandboxes.
    pub fn network_mode(&self) -> &str {
        match self {
            Plugin::Noop => "bridge",
            Plugin::Kubenet(k) => &k.bridge,
        }
    }

    /// `NetworkPlugin.Status`
    pub fn status(&self) -> Result<(), String> {
        match self {
            Plugin::Noop => Ok(()),
            Plugin::Kubenet(k) => match *k.net_config.lock().unwrap() {
                Some(_) => Ok(()),
                None => Err("kubenet does not have netConfig. This is most likely due to lack of PodCIDR".into()),
            },
        }
    }

    pub fn pod_cidr(&self) -> Option<IpNet> {
        match self {
            Plugin::Noop => None,
            Plugin::Kubenet(k) => *k.net_config.lock().unwrap(),
        }
    }

    /// `Event(NET_PLUGIN_EVENT_POD_CIDR_CHANGE)`: the first pod CIDR wins;
    /// later changes are ignored, as in kubenet.
    pub async fn pod_cidr_changed(&self, client: &DockerClient, pod_cidrs: &str) -> Result<()> {
        let Plugin::Kubenet(k) = self else { return Ok(()) };
        if k.net_config.lock().unwrap().is_some() {
            tracing::info!(pod_cidr = pod_cidrs, "Ignoring subsequent pod CIDR update to new cidr");
            return Ok(());
        }
        tracing::debug!(pod_cidr = pod_cidrs, "Kubenet: PodCIDR is set to new value");
        let mut v4 = None;
        for (idx, c) in pod_cidrs.split(',').map(str::trim).filter(|c| !c.is_empty()).enumerate() {
            let n: IpNet = c.parse().map_err(|e| anyhow!("Failed to generate CNI network config with cidr {c} at index {idx}: {e}"))?;
            if n.is_ipv4() && v4.is_none() {
                v4 = Some(n);
            } else {
                tracing::warn!(pod_cidr = c, "kubenet: only the IPv4 pod CIDR is used");
            }
        }
        let cidr = v4.ok_or_else(|| anyhow!("kubenet: no IPv4 pod CIDR in {pod_cidrs:?}"))?;
        k.ensure_bridge(client, &cidr).await?;
        *k.net_config.lock().unwrap() = Some(cidr);
        tracing::info!(bridge = %k.bridge, pod_cidr = %cidr, gateway = %cidr.nth(1), mtu = k.mtu, "kubenet: bridge configured for pod CIDR");
        Ok(())
    }
}

impl Kubenet {
    /// Creates (or adopts) the Docker network that backs the bridge.
    async fn ensure_bridge(&self, client: &DockerClient, cidr: &IpNet) -> Result<()> {
        let subnet = cidr.to_string();
        match client.call(Method::GET, &format!("/networks/{}", self.bridge), None).await {
            Ok(n) => {
                let current = n["IPAM"]["Config"].as_array().and_then(|c| c.first()).and_then(|c| c["Subnet"].as_str()).unwrap_or("");
                if current == subnet {
                    return Ok(());
                }
                let in_use = n["Containers"].as_object().is_some_and(|c| !c.is_empty());
                if in_use {
                    return Err(anyhow!(
                        "bridge network {} has subnet {current} but the pod CIDR is {subnet}, and containers still use it",
                        self.bridge
                    ));
                }
                tracing::info!(bridge = %self.bridge, old = current, new = %subnet, "kubenet: replacing bridge network with a new pod CIDR");
                client.call(Method::DELETE, &format!("/networks/{}", self.bridge), None).await?;
            }
            Err(e) if is_not_found(&e) => {}
            Err(e) => return Err(e),
        }
        client.call(Method::POST, "/networks/create", Some(&bridge_network(&self.bridge, cidr, self.mtu, &self.node))).await?;
        Ok(())
    }
}

/// Body of `POST /networks/create` for the kubenet bridge.
pub fn bridge_network(bridge: &str, cidr: &IpNet, mtu: i32, node: &str) -> Value {
    json!({
        "Name": bridge,
        "Driver": "bridge",
        "CheckDuplicate": true,
        "IPAM": {"Driver": "default", "Config": [{"Subnet": cidr.to_string(), "Gateway": cidr.nth(1).to_string()}]},
        "Options": {
            "com.docker.network.bridge.name": bridge,
            "com.docker.network.driver.mtu": mtu.to_string(),
            // Pod IPs are routable cluster-wide: no NAT between pods, no filtering of routed traffic.
            "com.docker.network.bridge.enable_ip_masquerade": "false",
            "com.docker.network.bridge.gateway_mode_ipv4": "nat-unprotected",
            "com.docker.network.bridge.enable_icc": "true",
        },
        "Labels": {NODE_LABEL: node},
    })
}

/// `findMinMTU`: smallest MTU of the interfaces that are up and neither
/// loopback nor point-to-point.
pub fn find_min_mtu() -> Option<i32> {
    const IFF_UP: u32 = 0x1;
    const IFF_LOOPBACK: u32 = 0x8;
    const IFF_POINTOPOINT: u32 = 0x10;
    let mut min = None;
    for e in std::fs::read_dir("/sys/class/net").ok()?.flatten() {
        let p = e.path();
        let flags =
            std::fs::read_to_string(p.join("flags")).ok().and_then(|f| u32::from_str_radix(f.trim().trim_start_matches("0x"), 16).ok());
        let mtu = std::fs::read_to_string(p.join("mtu")).ok().and_then(|m| m.trim().parse::<i32>().ok());
        if let (Some(f), Some(m)) = (flags, mtu) {
            if f & IFF_UP != 0 && f & (IFF_LOOPBACK | IFF_POINTOPOINT) == 0 && min.is_none_or(|x| m < x) {
                min = Some(m);
            }
        }
    }
    min.filter(|m| *m >= 576)
}

/// The resolv.conf the dockershim writes for a pod (`rewriteResolvFile`).
pub fn resolv_conf(servers: &[String], searches: &[String], options: &[String]) -> String {
    let mut lines: Vec<String> = servers.iter().map(|s| format!("nameserver {s}")).collect();
    if !searches.is_empty() {
        lines.push(format!("search {}", searches.join(" ")));
    }
    if !options.is_empty() {
        lines.push(format!("options {}", options.join(" ")));
    }
    if lines.is_empty() {
        String::new()
    } else {
        lines.join("\n") + "\n"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kubenet_status_needs_pod_cidr() {
        let p = Plugin::new(&NetworkConfig { plugin: KUBENET.into(), mtu: 1450, bridge: String::new() }, "n1").unwrap();
        assert_eq!(p.network_mode(), "cbr0");
        assert!(p.status().unwrap_err().contains("lack of PodCIDR"));
        assert!(Plugin::new(&NetworkConfig::default(), "n1").unwrap().status().is_ok());
        assert!(Plugin::new(&NetworkConfig { plugin: "cni".into(), ..Default::default() }, "n1").is_err());
    }

    #[test]
    fn bridge_body() {
        let b = bridge_network("cbr0", &"10.244.3.0/24".parse().unwrap(), 1450, "n1");
        assert_eq!(b["IPAM"]["Config"][0]["Gateway"], "10.244.3.1");
        assert_eq!(b["Options"]["com.docker.network.bridge.enable_ip_masquerade"], "false");
        assert_eq!(b["Options"]["com.docker.network.driver.mtu"], "1450");
    }

    #[test]
    fn resolv() {
        let s = resolv_conf(&["10.96.0.10".into()], &["default.svc.cluster.local".into(), "svc.cluster.local".into()], &["ndots:5".into()]);
        assert_eq!(s, "nameserver 10.96.0.10\nsearch default.svc.cluster.local svc.cluster.local\noptions ndots:5\n");
    }
}
