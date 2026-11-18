//! nftables proxy mode (`pkg/proxy/nftables/proxier.go`).
//!
//! Every sync renders the complete ruleset for one dedicated table and loads it
//! with a single `nft -f -` transaction (`add table` + `delete table` + the new
//! table), so the kernel switches atomically from the old rules to the new ones and
//! nothing outside the table is touched. Established connections keep their NAT
//! bindings in conntrack across reloads.
//!
//! ```text
//! nat-prerouting / nat-output ─▶ services
//!     ip daddr . l4proto . dport  vmap @service-ips       ─▶ service-<svc>  (ClusterIP)
//!                                                         ─▶ external-<svc> (externalIPs, LB IPs)
//!     <local address> l4proto . dport vmap @service-nodeports ─▶ external-<svc>
//! service-<svc>   masquerade off-cluster clients; [affinity]; numgen random mod N ─▶ endpoint-<ep>
//! local-<svc>     same, node-local endpoints only (internal/externalTrafficPolicy: Local)
//! external-<svc>  mark for masquerade (policy Cluster) ─▶ service-<svc> | local-<svc>
//! endpoint-<ep>   hairpin masquerade; [update affinity]; dnat to ip:port
//! nat-postrouting ─▶ masquerading: masquerade packets carrying mark 0x4000
//! filter-*        reject traffic to services without endpoints and to unknown ClusterIP ports
//! ```

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;
use std::net::Ipv4Addr;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;

use crate::endpoints::{categorize, Endpoint, EndpointsMap};
use crate::serviceport::{ServicePortInfo, ServicePortName};

/// Default table name (upstream uses `kube-proxy`; a distinct name keeps this
/// proxy's rules recognisable on a shared machine).
pub const DEFAULT_TABLE: &str = "rustkube-proxy";
/// `--masquerade-bit` 14.
pub const MASQ_MARK: &str = "0x4000";

#[derive(Clone, Debug)]
pub struct NftConfig {
    pub table: String,
    pub node_name: String,
    /// Pod CIDR(s) of the cluster; traffic to a ClusterIP from outside it is masqueraded.
    pub cluster_cidr: Option<String>,
    /// Masquerade all traffic sent to ClusterIPs.
    pub masquerade_all: bool,
    /// CIDRs whose local addresses accept NodePort traffic; empty = every non-loopback local address.
    pub nodeport_addresses: Vec<String>,
}

impl Default for NftConfig {
    fn default() -> Self {
        Self {
            table: DEFAULT_TABLE.into(),
            node_name: String::new(),
            cluster_cidr: None,
            masquerade_all: false,
            nodeport_addresses: vec![],
        }
    }
}

/// FNV-1a 64 → base32, first 8 characters (stands in for upstream's sha256/base32 prefix).
fn hash(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    (0..8).map(|i| ALPHABET[((h >> (59 - i * 5)) & 31) as usize] as char).collect()
}

fn port_id(spn: &ServicePortName) -> String {
    format!("{}/{}/{}/{}", spn.namespace, spn.name, spn.protocol.nft(), spn.port)
}

/// `service-ULMVA6XW-ns1/svc1/tcp/p80` (nft object names are limited to 255 bytes).
fn chain_name(prefix: &str, spn: &ServicePortName) -> String {
    let id = port_id(spn);
    let mut s = format!("{prefix}-{}-{id}", hash(&format!("{prefix}{id}")));
    s.truncate(200);
    s
}

fn endpoint_chain(spn: &ServicePortName, ep: &Endpoint) -> String {
    let id = format!("{}__{}/{}", port_id(spn), ep.ip, ep.port);
    let mut s = format!("endpoint-{}-{id}", hash(&id));
    s.truncate(200);
    s
}

fn affinity_set(spn: &ServicePortName, ep: &Endpoint) -> String {
    endpoint_chain(spn, ep).replacen("endpoint-", "affinity-", 1)
}

struct Builder<'a> {
    cfg: &'a NftConfig,
    service_ips: Vec<String>,
    service_nodeports: Vec<String>,
    no_endpoint_services: Vec<String>,
    no_endpoint_nodeports: Vec<String>,
    cluster_ips: BTreeSet<Ipv4Addr>,
    chains: Vec<(String, Vec<String>)>,
    sets: Vec<String>,
    done: HashSet<String>,
}

impl Builder<'_> {
    fn chain(&mut self, name: String, rules: Vec<String>) {
        if self.done.insert(name.clone()) {
            self.chains.push((name, rules));
        }
    }

    fn endpoint(&mut self, spn: &ServicePortName, svc: &ServicePortInfo, ep: &Endpoint) {
        let name = endpoint_chain(spn, ep);
        if self.done.contains(&name) {
            return;
        }
        // Hairpin: a pod reaching itself through the service must see a different source.
        let mut rules = vec![format!("ip saddr {} jump mark-for-masquerade", ep.ip)];
        if let Some(t) = svc.session_affinity_seconds {
            let set = affinity_set(spn, ep);
            self.sets.push(format!("set {set} {{ type ipv4_addr ; flags dynamic,timeout ; timeout {t}s ; }}"));
            rules.push(format!("update @{set} {{ ip saddr }}"));
        }
        rules.push(format!("meta l4proto {} dnat to {}:{}", spn.protocol.nft(), ep.ip, ep.port));
        self.chain(name, rules);
    }

    /// Load-balancing chain over `eps` (with session affinity and the ClusterIP masquerade rule).
    fn lb_chain(&mut self, name: String, spn: &ServicePortName, svc: &ServicePortInfo, eps: &[Endpoint]) {
        let mut rules = vec![];
        let proto = spn.protocol.nft();
        if self.cfg.masquerade_all {
            rules.push(format!("ip daddr {} {proto} dport {} jump mark-for-masquerade", svc.cluster_ip, svc.port));
        } else if let Some(cidr) = &self.cfg.cluster_cidr {
            rules.push(format!("ip daddr {} {proto} dport {} ip saddr != {cidr} jump mark-for-masquerade", svc.cluster_ip, svc.port));
        }
        if svc.session_affinity_seconds.is_some() {
            for ep in eps {
                rules.push(format!("ip saddr @{} goto {}", affinity_set(spn, ep), endpoint_chain(spn, ep)));
            }
        }
        if eps.len() == 1 {
            rules.push(format!("goto {}", endpoint_chain(spn, &eps[0])));
        } else {
            let arms: Vec<String> = eps.iter().enumerate().map(|(i, ep)| format!("{i} : goto {}", endpoint_chain(spn, ep))).collect();
            rules.push(format!("numgen random mod {} vmap {{ {} }}", eps.len(), arms.join(" , ")));
        }
        self.chain(name, rules);
    }

    fn service(&mut self, spn: &ServicePortName, svc: &ServicePortInfo, eps: &[Endpoint]) {
        let cat = categorize(svc, eps, &self.cfg.node_name);
        let proto = spn.protocol.nft();
        self.cluster_ips.insert(svc.cluster_ip);
        for ep in cat.cluster.iter().chain(cat.local.iter()) {
            self.endpoint(spn, svc, ep);
        }
        let cluster_chain = chain_name("service", spn);
        let local_chain = chain_name("local", spn);
        if !cat.cluster.is_empty() {
            self.lb_chain(cluster_chain.clone(), spn, svc, &cat.cluster);
        }
        if !cat.local.is_empty() {
            self.lb_chain(local_chain.clone(), spn, svc, &cat.local);
        }
        // With a Local policy and only remote endpoints, traffic is dropped rather
        // than rejected (the service has endpoints, just not here).
        let unavailable = if cat.cluster.is_empty() { "goto reject-chain" } else { "drop" };

        // ClusterIP.
        let key = format!("{} . {proto} . {}", svc.cluster_ip, svc.port);
        match (svc.internal_policy_local, cat.internal.is_empty()) {
            (_, true) => self.no_endpoint_services.push(format!("{key} : {unavailable}")),
            (true, false) => self.service_ips.push(format!("{key} : goto {local_chain}")),
            (false, false) => self.service_ips.push(format!("{key} : goto {cluster_chain}")),
        }

        // NodePort, externalIPs, load-balancer IPs.
        if !svc.externally_accessible() {
            return;
        }
        let ext_keys: Vec<String> =
            svc.external_ips.iter().chain(svc.load_balancer_ips.iter()).map(|ip| format!("{ip} . {proto} . {}", svc.port)).collect();
        let np_key = svc.node_port.map(|np| format!("{proto} . {np}"));
        if cat.external.is_empty() {
            for k in ext_keys {
                self.no_endpoint_services.push(format!("{k} : {unavailable}"));
            }
            if let Some(k) = np_key {
                self.no_endpoint_nodeports.push(format!("{k} : {unavailable}"));
            }
            return;
        }
        let external_chain = chain_name("external", spn);
        let rules = if svc.external_policy_local {
            // Source IP is preserved; only node-local endpoints. Traffic from pods and from
            // the node itself may still use any endpoint.
            let mut r = vec![];
            if !cat.cluster.is_empty() {
                if let Some(cidr) = &self.cfg.cluster_cidr {
                    r.push(format!("ip saddr {cidr} goto {cluster_chain}"));
                }
                r.push("fib saddr type local jump mark-for-masquerade".to_string());
                r.push(format!("fib saddr type local goto {cluster_chain}"));
            }
            r.push(format!("goto {local_chain}"));
            r
        } else {
            vec!["jump mark-for-masquerade".to_string(), format!("goto {cluster_chain}")]
        };
        self.chain(external_chain.clone(), rules);
        for k in ext_keys {
            self.service_ips.push(format!("{k} : goto {external_chain}"));
        }
        if let Some(k) = np_key {
            self.service_nodeports.push(format!("{k} : goto {external_chain}"));
        }
    }
}

fn elements<T: AsRef<str>>(items: &[T]) -> String {
    if items.is_empty() {
        String::new()
    } else {
        format!(" elements = {{ {} }} ;", items.iter().map(AsRef::as_ref).collect::<Vec<_>>().join(", "))
    }
}

/// Renders the full `nft -f` script for the given services and endpoints.
pub fn ruleset(cfg: &NftConfig, services: &BTreeMap<ServicePortName, ServicePortInfo>, endpoints: &EndpointsMap) -> String {
    let mut b = Builder {
        cfg,
        service_ips: vec![],
        service_nodeports: vec![],
        no_endpoint_services: vec![],
        no_endpoint_nodeports: vec![],
        cluster_ips: BTreeSet::new(),
        chains: vec![],
        sets: vec![],
        done: HashSet::new(),
    };
    for (spn, svc) in services {
        b.service(spn, svc, endpoints.get(spn).map(Vec::as_slice).unwrap_or(&[]));
    }
    let nodeport_match = if cfg.nodeport_addresses.is_empty() {
        "fib daddr type local ip daddr != 127.0.0.0/8".to_string()
    } else {
        "ip daddr @nodeport-ips".to_string()
    };
    let t = &cfg.table;
    let mut s = String::new();
    // Create-then-delete makes the delete succeed on the first run; the whole file is one transaction.
    let _ = writeln!(s, "add table ip {t}\ndelete table ip {t}\ntable ip {t} {{");
    let _ = writeln!(s, "\tcomment \"rules for kube-proxy (rustkubernetes)\"");
    let cluster_ips: Vec<String> = b.cluster_ips.iter().map(ToString::to_string).collect();
    let _ = writeln!(s, "\tset cluster-ips {{ type ipv4_addr ;{} }}", elements(&cluster_ips));
    if !cfg.nodeport_addresses.is_empty() {
        let _ = writeln!(s, "\tset nodeport-ips {{ type ipv4_addr ; flags interval ; auto-merge ;{} }}", elements(&cfg.nodeport_addresses));
    }
    let _ = writeln!(s, "\tmap service-ips {{ type ipv4_addr . inet_proto . inet_service : verdict ;{} }}", elements(&b.service_ips));
    let _ = writeln!(s, "\tmap service-nodeports {{ type inet_proto . inet_service : verdict ;{} }}", elements(&b.service_nodeports));
    let _ = writeln!(
        s,
        "\tmap no-endpoint-services {{ type ipv4_addr . inet_proto . inet_service : verdict ;{} }}",
        elements(&b.no_endpoint_services)
    );
    let _ =
        writeln!(s, "\tmap no-endpoint-nodeports {{ type inet_proto . inet_service : verdict ;{} }}", elements(&b.no_endpoint_nodeports));
    for set in &b.sets {
        let _ = writeln!(s, "\t{set}");
    }
    let base: [(&str, &str, &[&str]); 8] = [
        (
            "filter-input",
            "type filter hook input priority -10 ; policy accept ;",
            &["ct state new jump nodeport-endpoints-check", "ct state new jump service-endpoints-check"],
        ),
        (
            "filter-forward",
            "type filter hook forward priority -10 ; policy accept ;",
            &["ct state new jump service-endpoints-check", "ct state new jump cluster-ips-check"],
        ),
        ("filter-output", "type filter hook output priority -110 ; policy accept ;", &["ct state new jump service-endpoints-check"]),
        ("filter-output-post-dnat", "type filter hook output priority -90 ; policy accept ;", &["ct state new jump cluster-ips-check"]),
        ("nat-prerouting", "type nat hook prerouting priority dstnat ; policy accept ;", &["jump services"]),
        ("nat-output", "type nat hook output priority -100 ; policy accept ;", &["jump services"]),
        ("nat-postrouting", "type nat hook postrouting priority srcnat ; policy accept ;", &["jump masquerading"]),
        ("reject-chain", "", &["reject"]),
    ];
    for (name, hook, rules) in base {
        let _ = writeln!(s, "\tchain {name} {{");
        if !hook.is_empty() {
            let _ = writeln!(s, "\t\t{hook}");
        }
        for r in rules {
            let _ = writeln!(s, "\t\t{r}");
        }
        let _ = writeln!(s, "\t}}");
    }
    let fixed: Vec<(String, Vec<String>)> = vec![
        ("service-endpoints-check".into(), vec!["ip daddr . meta l4proto . th dport vmap @no-endpoint-services".into()]),
        ("nodeport-endpoints-check".into(), vec![format!("{nodeport_match} meta l4proto . th dport vmap @no-endpoint-nodeports")]),
        ("cluster-ips-check".into(), vec!["ip daddr @cluster-ips reject comment \"invalid ClusterIP port\"".into()]),
        ("mark-for-masquerade".into(), vec![format!("meta mark set meta mark | {MASQ_MARK}")]),
        (
            "masquerading".into(),
            vec![
                format!("meta mark & {MASQ_MARK} == 0 return"),
                format!("meta mark set meta mark ^ {MASQ_MARK}"),
                "masquerade fully-random".into(),
            ],
        ),
        (
            "services".into(),
            vec![
                "ip daddr . meta l4proto . th dport vmap @service-ips".into(),
                format!("{nodeport_match} meta l4proto . th dport vmap @service-nodeports"),
            ],
        ),
    ];
    for (name, rules) in fixed.iter().chain(b.chains.iter()) {
        let _ = writeln!(s, "\tchain {name} {{");
        for r in rules {
            let _ = writeln!(s, "\t\t{r}");
        }
        let _ = writeln!(s, "\t}}");
    }
    s.push_str("}\n");
    s
}

/// Applies scripts with the `nft` tool (upstream uses `knftables`, which also execs `nft`).
#[async_trait]
pub trait Nft: Send + Sync {
    async fn apply(&self, script: &str) -> anyhow::Result<()>;
    async fn table_exists(&self, table: &str) -> anyhow::Result<bool>;
    async fn delete_table(&self, table: &str) -> anyhow::Result<()>;
}

pub struct NftCommand {
    pub path: String,
}

impl NftCommand {
    async fn run(&self, args: &[&str], stdin: Option<&str>) -> anyhow::Result<std::process::Output> {
        let mut cmd = tokio::process::Command::new(&self.path);
        cmd.args(args)
            .stdin(if stdin.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("running {}: {e}", self.path))?;
        if let (Some(input), Some(mut w)) = (stdin, child.stdin.take()) {
            w.write_all(input.as_bytes()).await?;
            drop(w);
        }
        Ok(child.wait_with_output().await?)
    }
}

#[async_trait]
impl Nft for NftCommand {
    async fn apply(&self, script: &str) -> anyhow::Result<()> {
        let out = self.run(&["-f", "-"], Some(script)).await?;
        if !out.status.success() {
            anyhow::bail!("nft -f failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    async fn table_exists(&self, table: &str) -> anyhow::Result<bool> {
        let out = self.run(&["list", "tables", "ip"], None).await?;
        if !out.status.success() {
            anyhow::bail!("nft list tables failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).lines().any(|l| l.trim() == format!("table ip {table}")))
    }

    async fn delete_table(&self, table: &str) -> anyhow::Result<()> {
        if self.table_exists(table).await? {
            let out = self.run(&["delete", "table", "ip", table], None).await?;
            if !out.status.success() {
                anyhow::bail!("nft delete table failed: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
        }
        Ok(())
    }
}

/// Logs scripts instead of applying them (`--dry-run`): no privileges needed.
pub struct NftDryRun;

#[async_trait]
impl Nft for NftDryRun {
    async fn apply(&self, script: &str) -> anyhow::Result<()> {
        println!("{script}");
        Ok(())
    }

    async fn table_exists(&self, _table: &str) -> anyhow::Result<bool> {
        Ok(true)
    }

    async fn delete_table(&self, _table: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::endpoints_map;
    use crate::serviceport::service_ports;
    use k8s_api::core::v1::Service;
    use k8s_api::discovery::v1::EndpointSlice;
    use serde_json::json;

    fn build(cfg: &NftConfig, svcs: Vec<serde_json::Value>, slices: Vec<serde_json::Value>) -> String {
        let mut services = BTreeMap::new();
        for s in svcs {
            let svc: Service = serde_json::from_value(s).unwrap();
            for p in service_ports(&svc) {
                services.insert(p.name.clone(), p);
            }
        }
        let slices: Vec<EndpointSlice> = slices.into_iter().map(|s| serde_json::from_value(s).unwrap()).collect();
        ruleset(cfg, &services, &endpoints_map(slices.iter()))
    }

    fn slice(svc: &str, port: i64, ips: &[(&str, &str)]) -> serde_json::Value {
        json!({
            "metadata": {"namespace": "ns", "name": format!("{svc}-x"), "labels": {"kubernetes.io/service-name": svc}},
            "addressType": "IPv4",
            "ports": [{"name": "http", "port": port, "protocol": "TCP"}],
            "endpoints": ips.iter().map(|(ip, node)| json!({"addresses": [ip], "conditions": {"ready": true}, "nodeName": node})).collect::<Vec<_>>()
        })
    }

    fn cfg() -> NftConfig {
        NftConfig { node_name: "n1".into(), cluster_cidr: Some("172.17.0.0/16".into()), ..Default::default() }
    }

    #[test]
    fn hash_is_stable() {
        assert_eq!(hash("x"), hash("x"));
        assert_ne!(hash("a"), hash("b"));
        assert_eq!(hash("x").len(), 8);
    }

    #[test]
    fn cluster_ip_and_nodeport() {
        let s = build(
            &cfg(),
            vec![json!({"metadata": {"namespace": "ns", "name": "web"},
                        "spec": {"type": "NodePort", "clusterIP": "10.96.0.10", "ports": [{"name": "http", "port": 80, "nodePort": 30080}]}})],
            vec![slice("web", 8080, &[("172.17.0.2", "n1"), ("172.17.0.3", "n1")])],
        );
        assert!(s.starts_with("add table ip rustkube-proxy\ndelete table ip rustkube-proxy\n"));
        let svc_chain = chain_name(
            "service",
            &ServicePortName {
                namespace: "ns".into(),
                name: "web".into(),
                port: "http".into(),
                protocol: crate::serviceport::Protocol::Tcp,
            },
        );
        assert!(s.contains(&format!("10.96.0.10 . tcp . 80 : goto {svc_chain}")), "{s}");
        assert!(s.contains("tcp . 30080 : goto external-"));
        assert!(s.contains("numgen random mod 2 vmap { 0 : goto endpoint-"));
        assert!(s.contains("ip daddr 10.96.0.10 tcp dport 80 ip saddr != 172.17.0.0/16 jump mark-for-masquerade"));
        assert!(s.contains("meta l4proto tcp dnat to 172.17.0.3:8080"));
        assert!(s.contains("ip saddr 172.17.0.2 jump mark-for-masquerade"));
        assert!(s.contains("set cluster-ips { type ipv4_addr ; elements = { 10.96.0.10 } ; }"));
        // External chain masquerades (externalTrafficPolicy: Cluster).
        assert!(s.contains("\t\tjump mark-for-masquerade\n\t\tgoto service-"));
    }

    #[test]
    fn no_endpoints_rejects() {
        let s = build(
            &cfg(),
            vec![json!({"metadata": {"namespace": "ns", "name": "web"},
                        "spec": {"type": "NodePort", "clusterIP": "10.96.0.10", "ports": [{"name": "http", "port": 80, "nodePort": 30080}]}})],
            vec![],
        );
        assert!(s.contains("map no-endpoint-services { type ipv4_addr . inet_proto . inet_service : verdict ; elements = { 10.96.0.10 . tcp . 80 : goto reject-chain } ; }"));
        assert!(s.contains("elements = { tcp . 30080 : goto reject-chain }"));
        assert!(!s.contains("goto service-") && !s.contains("chain endpoint-"));
    }

    #[test]
    fn local_policies_and_affinity() {
        let s = build(
            &cfg(),
            vec![json!({"metadata": {"namespace": "ns", "name": "web"},
                        "spec": {"type": "NodePort", "clusterIP": "10.96.0.10", "externalTrafficPolicy": "Local",
                                 "internalTrafficPolicy": "Local", "sessionAffinity": "ClientIP",
                                 "sessionAffinityConfig": {"clientIP": {"timeoutSeconds": 60}},
                                 "ports": [{"name": "http", "port": 80, "nodePort": 30080}]}})],
            vec![slice("web", 8080, &[("172.17.0.2", "n1"), ("172.17.0.9", "n2")])],
        );
        assert!(s.contains("10.96.0.10 . tcp . 80 : goto local-"), "{s}");
        assert!(s.contains("flags dynamic,timeout ; timeout 60s"));
        assert!(s.contains("update @affinity-"));
        assert!(s.contains("fib saddr type local goto service-"));
        assert!(!s.contains("dnat to 172.17.0.9:8080\n\t}\n\tchain local"));
        // Only the remote endpoint exists → traffic dropped (not rejected).
        let s = build(
            &cfg(),
            vec![json!({"metadata": {"namespace": "ns", "name": "web"},
                        "spec": {"clusterIP": "10.96.0.10", "internalTrafficPolicy": "Local", "ports": [{"name": "http", "port": 80}]}})],
            vec![slice("web", 8080, &[("172.17.0.9", "n2")])],
        );
        assert!(s.contains("10.96.0.10 . tcp . 80 : drop"));
    }

    #[test]
    fn nodeport_addresses() {
        let c = NftConfig { nodeport_addresses: vec!["192.168.1.0/24".into()], ..cfg() };
        let s = build(&c, vec![], vec![]);
        assert!(s.contains("set nodeport-ips { type ipv4_addr ; flags interval ; auto-merge ; elements = { 192.168.1.0/24 } ; }"));
        assert!(s.contains("ip daddr @nodeport-ips meta l4proto . th dport vmap @service-nodeports"));
    }
}
