//! Service ports as the proxy sees them (`pkg/proxy/serviceport.go`,
//! `servicechangetracker.go: newBaseServiceInfo`).

use std::fmt;
use std::net::Ipv4Addr;

use k8s_api::core::v1::Service;

/// Services carrying this label are handled by another proxy (`service.kubernetes.io/service-proxy-name`).
pub const LABEL_SERVICE_PROXY_NAME: &str = "service.kubernetes.io/service-proxy-name";
/// Headless marker set on EndpointSlices / Services (`service.kubernetes.io/headless`).
pub const LABEL_HEADLESS: &str = "service.kubernetes.io/headless";
/// `DefaultClientIPServiceAffinitySeconds`.
pub const DEFAULT_AFFINITY_SECONDS: u32 = 10800;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Protocol {
    Tcp,
    Udp,
    Sctp,
}

impl Protocol {
    pub fn parse(s: Option<&str>) -> Option<Self> {
        match s.unwrap_or("TCP") {
            "TCP" => Some(Self::Tcp),
            "UDP" => Some(Self::Udp),
            "SCTP" => Some(Self::Sctp),
            _ => None,
        }
    }

    /// nftables protocol keyword.
    pub fn nft(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
            Self::Sctp => "sctp",
        }
    }
}

/// `proxy.ServicePortName`: namespace/name:port + protocol.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ServicePortName {
    pub namespace: String,
    pub name: String,
    pub port: String,
    pub protocol: Protocol,
}

impl fmt::Display for ServicePortName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.namespace, self.name)?;
        if !self.port.is_empty() {
            write!(f, ":{}", self.port)?;
        }
        Ok(())
    }
}

/// Everything needed to program one service port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServicePortInfo {
    pub name: ServicePortName,
    pub cluster_ip: Ipv4Addr,
    pub port: u16,
    pub node_port: Option<u16>,
    pub external_ips: Vec<Ipv4Addr>,
    pub load_balancer_ips: Vec<Ipv4Addr>,
    /// `Some(timeout)` when `sessionAffinity: ClientIP`.
    pub session_affinity_seconds: Option<u32>,
    /// `externalTrafficPolicy: Local` (only meaningful for externally reachable ports).
    pub external_policy_local: bool,
    /// `internalTrafficPolicy: Local`.
    pub internal_policy_local: bool,
}

impl ServicePortInfo {
    /// NodePort, externalIPs or load-balancer IPs: reachable from outside the cluster.
    pub fn externally_accessible(&self) -> bool {
        self.node_port.is_some() || !self.external_ips.is_empty() || !self.load_balancer_ips.is_empty()
    }
}

fn ipv4(s: &str) -> Option<Ipv4Addr> {
    s.parse().ok()
}

/// The proxied ports of a Service; empty for ExternalName, headless and
/// non-IPv4 services and services owned by another proxy.
pub fn service_ports(svc: &Service) -> Vec<ServicePortInfo> {
    let labels = svc.metadata.labels.clone().unwrap_or_default();
    if labels.contains_key(LABEL_SERVICE_PROXY_NAME) || labels.contains_key(LABEL_HEADLESS) {
        return vec![];
    }
    let Some(spec) = &svc.spec else { return vec![] };
    if spec.type_.as_deref() == Some("ExternalName") {
        return vec![];
    }
    let Some(cluster_ip) = spec.cluster_ip.as_deref().and_then(ipv4) else {
        return vec![]; // headless ("None"), unallocated or IPv6
    };
    let external_ips: Vec<Ipv4Addr> = spec.external_ips.iter().flatten().filter_map(|s| ipv4(s)).collect();
    let load_balancer_ips: Vec<Ipv4Addr> = svc
        .status
        .as_ref()
        .and_then(|s| s.load_balancer.as_ref())
        .and_then(|lb| lb["ingress"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|i| i["ip"].as_str().and_then(ipv4))
        .collect();
    let affinity = (spec.session_affinity.as_deref() == Some("ClientIP")).then(|| {
        spec.session_affinity_config
            .as_ref()
            .and_then(|c| c.client_ip.as_ref())
            .and_then(|c| c.timeout_seconds)
            .filter(|t| *t > 0)
            .map(|t| t as u32)
            .unwrap_or(DEFAULT_AFFINITY_SECONDS)
    });
    let ns = svc.metadata.namespace.clone().unwrap_or_default();
    let name = svc.metadata.name.clone().unwrap_or_default();
    let node_ports_enabled = matches!(spec.type_.as_deref(), Some("NodePort") | Some("LoadBalancer"));
    spec.ports
        .iter()
        .flatten()
        .filter_map(|p| {
            let protocol = Protocol::parse(p.protocol.as_deref())?;
            let port = u16::try_from(p.port).ok().filter(|p| *p != 0)?;
            let node_port = if node_ports_enabled { p.node_port.and_then(|n| u16::try_from(n).ok()).filter(|n| *n != 0) } else { None };
            let mut info = ServicePortInfo {
                name: ServicePortName { namespace: ns.clone(), name: name.clone(), port: p.name.clone().unwrap_or_default(), protocol },
                cluster_ip,
                port,
                node_port,
                external_ips: external_ips.clone(),
                load_balancer_ips: load_balancer_ips.clone(),
                session_affinity_seconds: affinity,
                external_policy_local: false,
                internal_policy_local: spec.internal_traffic_policy.as_deref() == Some("Local"),
            };
            info.external_policy_local = info.externally_accessible() && spec.external_traffic_policy.as_deref() == Some("Local");
            Some(info)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(v: serde_json::Value) -> Service {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn ports_and_policies() {
        let s = svc(serde_json::json!({
            "metadata": {"namespace": "ns", "name": "web"},
            "spec": {"type": "NodePort", "clusterIP": "10.96.0.10", "externalIPs": ["192.0.2.1"],
                     "sessionAffinity": "ClientIP", "externalTrafficPolicy": "Local",
                     "ports": [{"name": "http", "port": 80, "nodePort": 30080}, {"name": "dns", "port": 53, "protocol": "UDP"}]},
            "status": {"loadBalancer": {"ingress": [{"ip": "198.51.100.7"}]}}
        }));
        let ps = service_ports(&s);
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0].name.to_string(), "ns/web:http");
        assert_eq!(ps[0].node_port, Some(30080));
        assert_eq!(ps[0].session_affinity_seconds, Some(DEFAULT_AFFINITY_SECONDS));
        assert!(ps[0].external_policy_local);
        assert_eq!(ps[1].name.protocol, Protocol::Udp);
        assert_eq!(ps[0].load_balancer_ips, vec!["198.51.100.7".parse::<Ipv4Addr>().unwrap()]);
    }

    #[test]
    fn skipped_services() {
        let headless = svc(serde_json::json!({"metadata": {"name": "h"}, "spec": {"clusterIP": "None", "ports": [{"port": 80}]}}));
        let ext = svc(serde_json::json!({"metadata": {"name": "e"}, "spec": {"type": "ExternalName", "externalName": "example.com"}}));
        let other = svc(serde_json::json!({"metadata": {"name": "o", "labels": {LABEL_SERVICE_PROXY_NAME: "x"}},
                                            "spec": {"clusterIP": "10.96.0.3", "ports": [{"port": 80}]}}));
        assert!(service_ports(&headless).is_empty() && service_ports(&ext).is_empty() && service_ports(&other).is_empty());
    }
}
