//! Endpoints per service port, built from EndpointSlices (`pkg/proxy/endpoint.go`,
//! `endpointslicecache.go`), and their categorisation by traffic policy
//! (`pkg/proxy/topology.go: CategorizeEndpoints`).

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use k8s_api::discovery::v1::{EndpointSlice, LABEL_SERVICE_NAME};

use crate::serviceport::{Protocol, ServicePortInfo, ServicePortName};

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Endpoint {
    pub ip: Ipv4Addr,
    pub port: u16,
    pub ready: bool,
    pub serving: bool,
    pub terminating: bool,
    pub node_name: Option<String>,
}

impl Endpoint {
    pub fn is_local(&self, node: &str) -> bool {
        self.node_name.as_deref() == Some(node)
    }

    pub fn addr(&self) -> String {
        format!("{}:{}", self.ip, self.port)
    }
}

pub type EndpointsMap = BTreeMap<ServicePortName, Vec<Endpoint>>;

/// Merges all IPv4 slices into endpoints per service port, de-duplicated by ip:port
/// (an address may briefly appear in two slices while the controller moves it).
pub fn endpoints_map<'a>(slices: impl IntoIterator<Item = &'a EndpointSlice>) -> EndpointsMap {
    let mut map = EndpointsMap::new();
    for slice in slices {
        if slice.address_type != "IPv4" {
            continue;
        }
        let Some(svc) = slice.metadata.labels.as_ref().and_then(|l| l.get(LABEL_SERVICE_NAME)) else { continue };
        let ns = slice.metadata.namespace.clone().unwrap_or_default();
        for port in slice.ports.iter().flatten() {
            let (Some(p), Some(protocol)) = (port.port.and_then(|p| u16::try_from(p).ok()), Protocol::parse(port.protocol.as_deref()))
            else {
                continue;
            };
            let key = ServicePortName { namespace: ns.clone(), name: svc.clone(), port: port.name.clone().unwrap_or_default(), protocol };
            let list = map.entry(key).or_default();
            for ep in &slice.endpoints {
                let Some(ip) = ep.addresses.first().and_then(|a| a.parse().ok()) else { continue };
                let c = ep.conditions.clone().unwrap_or_default();
                // A nil `ready` means ready; a nil `serving` follows `ready`.
                let ready = c.ready.unwrap_or(true);
                list.push(Endpoint {
                    ip,
                    port: p,
                    ready,
                    serving: c.serving.unwrap_or(ready),
                    terminating: c.terminating.unwrap_or(false),
                    node_name: ep.node_name.clone(),
                });
            }
        }
    }
    for list in map.values_mut() {
        list.sort_by_key(|e| (e.ip, e.port, !e.ready));
        list.dedup_by(|a, b| a.ip == b.ip && a.port == b.port);
    }
    map.retain(|_, v| !v.is_empty());
    map
}

/// Ready endpoints, or — when none is ready — those still serving while terminating,
/// so connections drain during rolling updates (`ProxyTerminatingEndpoints`).
fn usable<'a>(eps: impl Iterator<Item = &'a Endpoint> + Clone) -> Vec<Endpoint> {
    let ready: Vec<Endpoint> = eps.clone().filter(|e| e.ready).cloned().collect();
    if !ready.is_empty() {
        return ready;
    }
    eps.filter(|e| e.serving && e.terminating).cloned().collect()
}

/// Endpoints to use for each kind of traffic.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Categorized {
    /// All usable endpoints (`clusterEndpoints`).
    pub cluster: Vec<Endpoint>,
    /// Usable endpoints on this node (`localEndpoints`), only computed when a policy is `Local`.
    pub local: Vec<Endpoint>,
    /// Endpoints for traffic to the cluster IP.
    pub internal: Vec<Endpoint>,
    /// Endpoints for NodePort / external IP / load-balancer traffic.
    pub external: Vec<Endpoint>,
}

pub fn categorize(svc: &ServicePortInfo, eps: &[Endpoint], node: &str) -> Categorized {
    let cluster = usable(eps.iter());
    let local =
        if svc.internal_policy_local || svc.external_policy_local { usable(eps.iter().filter(|e| e.is_local(node))) } else { vec![] };
    Categorized {
        internal: if svc.internal_policy_local { local.clone() } else { cluster.clone() },
        external: if svc.external_policy_local { local.clone() } else { cluster.clone() },
        cluster,
        local,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice(v: serde_json::Value) -> EndpointSlice {
        serde_json::from_value(v).unwrap()
    }

    fn ep(ip: &str, ready: bool, terminating: bool, node: &str) -> Endpoint {
        Endpoint { ip: ip.parse().unwrap(), port: 80, ready, serving: ready || terminating, terminating, node_name: Some(node.into()) }
    }

    #[test]
    fn merges_slices() {
        let a = slice(serde_json::json!({
            "metadata": {"namespace": "ns", "name": "web-a", "labels": {LABEL_SERVICE_NAME: "web"}},
            "addressType": "IPv4",
            "ports": [{"name": "http", "port": 8080, "protocol": "TCP"}],
            "endpoints": [{"addresses": ["10.0.0.2"], "conditions": {"ready": true}, "nodeName": "n1"},
                          {"addresses": ["10.0.0.1"], "conditions": {"ready": false, "serving": true, "terminating": true}}]
        }));
        let b = slice(serde_json::json!({
            "metadata": {"namespace": "ns", "name": "web-b", "labels": {LABEL_SERVICE_NAME: "web"}},
            "addressType": "IPv4",
            "ports": [{"name": "http", "port": 8080, "protocol": "TCP"}],
            "endpoints": [{"addresses": ["10.0.0.2"]}, {"addresses": ["10.0.0.3"]}]
        }));
        let ipv6 = slice(serde_json::json!({"metadata": {"labels": {LABEL_SERVICE_NAME: "web"}}, "addressType": "IPv6", "endpoints": []}));
        let m = endpoints_map([&a, &b, &ipv6]);
        assert_eq!(m.len(), 1);
        let eps = m.values().next().unwrap();
        let ips: Vec<String> = eps.iter().map(Endpoint::addr).collect();
        assert_eq!(ips, ["10.0.0.1:8080", "10.0.0.2:8080", "10.0.0.3:8080"]);
        assert!(!eps[0].ready && eps[0].terminating);
    }

    #[test]
    fn categorize_policies_and_terminating_fallback() {
        let mut svc = crate::serviceport::service_ports(
            &serde_json::from_value(
                serde_json::json!({"metadata": {"name": "s"}, "spec": {"clusterIP": "10.96.0.9", "ports": [{"port": 80}]}}),
            )
            .unwrap(),
        )
        .remove(0);
        let eps = vec![ep("10.0.0.1", true, false, "n1"), ep("10.0.0.2", true, false, "n2"), ep("10.0.0.3", false, true, "n1")];
        let c = categorize(&svc, &eps, "n1");
        assert_eq!(c.internal.len(), 2);
        svc.internal_policy_local = true;
        let c = categorize(&svc, &eps, "n1");
        assert_eq!(c.internal.iter().map(|e| e.ip.to_string()).collect::<Vec<_>>(), ["10.0.0.1"]);
        // Only terminating endpoints left: they are still used.
        let c = categorize(&svc, &[ep("10.0.0.3", false, true, "n1")], "n1");
        assert_eq!(c.internal.len(), 1);
        let c = categorize(&svc, &[ep("10.0.0.4", false, false, "n1")], "n1");
        assert!(c.internal.is_empty());
    }
}
