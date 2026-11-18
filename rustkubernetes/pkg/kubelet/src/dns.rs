//! Pod DNS (`pkg/kubelet/network/dns`): resolver settings from `dnsPolicy`,
//! `dnsConfig`, `--cluster-dns` and `--cluster-domain`.
//!
//! | `dnsPolicy` | Resolver |
//! |-------------|----------|
//! | `ClusterFirst` (default) | cluster DNS, search `<ns>.svc.<domain> svc.<domain> <domain>`, `ndots:5`; host resolver for `hostNetwork` pods |
//! | `ClusterFirstWithHostNet` | cluster DNS even for `hostNetwork` pods |
//! | `Default` | the node's resolver |
//! | `None` | only `dnsConfig` |
//!
//! `dnsConfig` nameservers/searches are appended and options override by name.

use k8s_api::core::v1::Pod;
use serde_json::Value;

use crate::runtime::DnsConfig;

pub fn pod_dns(pod: &Pod, cluster_dns: &[String], cluster_domain: &str) -> Option<DnsConfig> {
    let spec = pod.spec.clone().unwrap_or_default();
    let host_net = spec.host_network.unwrap_or(false);
    let policy = spec.dns_policy.as_deref().unwrap_or("ClusterFirst");
    let ns = pod.metadata.namespace();
    let mut cfg = match policy {
        "None" => DnsConfig::default(),
        "Default" => return merge(None, spec.extra.get("dnsConfig")),
        "ClusterFirst" if host_net => return merge(None, spec.extra.get("dnsConfig")),
        _ if cluster_dns.is_empty() => {
            tracing::warn!(pod = %pod.metadata.name(), "pod has dnsPolicy {policy} but the kubelet has no --cluster-dns: using the node resolver");
            return merge(None, spec.extra.get("dnsConfig"));
        }
        _ => DnsConfig {
            servers: cluster_dns.to_vec(),
            searches: vec![format!("{ns}.svc.{cluster_domain}"), format!("svc.{cluster_domain}"), cluster_domain.to_string()],
            options: vec!["ndots:5".into()],
        },
    };
    if policy == "None" {
        cfg = DnsConfig::default();
    }
    merge(Some(cfg), spec.extra.get("dnsConfig"))
}

fn merge(base: Option<DnsConfig>, extra: Option<&Value>) -> Option<DnsConfig> {
    let Some(x) = extra.filter(|v| v.is_object()) else { return base };
    let mut c = base.unwrap_or_default();
    let strs = |k: &str| x[k].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>();
    for s in strs("nameservers") {
        if !c.servers.contains(&s) {
            c.servers.push(s);
        }
    }
    for s in strs("searches") {
        if !c.searches.contains(&s) {
            c.searches.push(s);
        }
    }
    for o in x["options"].as_array().into_iter().flatten() {
        let name = o["name"].as_str().unwrap_or("");
        if name.is_empty() {
            continue;
        }
        c.options.retain(|e| e.split(':').next() != Some(name));
        c.options.push(match o["value"].as_str() {
            Some(v) => format!("{name}:{v}"),
            None => name.to_string(),
        });
    }
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(spec: serde_json::Value) -> Pod {
        serde_json::from_value(serde_json::json!({"metadata": {"name": "p", "namespace": "dev"}, "spec": spec})).unwrap()
    }

    #[test]
    fn policies() {
        let dns = vec!["10.96.0.10".to_string()];
        let c = pod_dns(&pod(serde_json::json!({"containers": []})), &dns, "cluster.local").unwrap();
        assert_eq!(c.servers, ["10.96.0.10"]);
        assert_eq!(c.searches, ["dev.svc.cluster.local", "svc.cluster.local", "cluster.local"]);
        assert_eq!(c.options, ["ndots:5"]);
        assert!(pod_dns(&pod(serde_json::json!({"containers": [], "dnsPolicy": "Default"})), &dns, "cluster.local").is_none());
        assert!(pod_dns(&pod(serde_json::json!({"containers": [], "hostNetwork": true})), &dns, "cluster.local").is_none());
        let c = pod_dns(
            &pod(serde_json::json!({"containers": [], "dnsPolicy": "None",
            "dnsConfig": {"nameservers": ["1.1.1.1"], "options": [{"name": "ndots", "value": "2"}, {"name": "edns0"}]}})),
            &dns,
            "cluster.local",
        )
        .unwrap();
        assert_eq!(
            (c.servers, c.searches, c.options),
            (vec!["1.1.1.1".to_string()], vec![], vec!["ndots:2".to_string(), "edns0".to_string()])
        );
        let c = pod_dns(
            &pod(serde_json::json!({"containers": [], "dnsConfig": {"options": [{"name": "ndots", "value": "1"}]}})),
            &dns,
            "cluster.local",
        )
        .unwrap();
        assert_eq!(c.options, ["ndots:1"]);
    }
}
