//! `kubeadm` (subset): generates the PKI and kubeconfig files a secure cluster
//! needs, in kubeadm's layout (`<dir>/pki/*`, `<dir>/*.conf`).
//!
//! ```text
//! kubeadm init phase certs all       --cert-dir DIR/pki ...
//! kubeadm init phase kubeconfig all  --cert-dir DIR/pki --kubeconfig-dir DIR ...
//! kubeadm init                       both of the above
//! kubeadm kubeconfig user --client-name jane [--org dev]   (kubeconfig on stdout)
//! ```
//!
//! Static pods, etcd and add-on installation are done by `hack/local-up-cluster.sh`.

mod app;

use std::net::IpAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use app::phases::{certs, kubeconfig};

#[derive(Parser)]
#[command(name = "kubeadm", version, about = "kubeadm: easily bootstrap a secure Kubernetes cluster (certificate and kubeconfig phases)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run all init phases (certs + kubeconfig).
    Init(InitOpts),
    /// Kubeconfig file utilities.
    Kubeconfig {
        #[command(subcommand)]
        cmd: KubeconfigCmd,
    },
    /// Credentials for one more node (worker): kubelet serving cert + key and
    /// `kubelet.conf` (CN=system:node:NAME, O=system:nodes), signed by the cluster CA.
    NodeCredentials {
        #[arg(long)]
        node_name: String,
        #[arg(long, value_delimiter = ',')]
        node_ip: Vec<IpAddr>,
        /// Directory receiving kubelet.crt, kubelet.key, kubelet.conf and ca.crt.
        #[arg(long)]
        out: PathBuf,
        #[command(flatten)]
        common: Common,
    },
}

#[derive(Subcommand)]
enum KubeconfigCmd {
    /// Output a kubeconfig file for an additional user.
    User {
        #[arg(long)]
        client_name: String,
        /// Organizations (groups) of the client certificate.
        #[arg(long = "org")]
        orgs: Vec<String>,
        #[command(flatten)]
        common: Common,
    },
}

#[derive(Args, Clone)]
struct Common {
    /// Directory holding (or receiving) certificates.
    #[arg(long, default_value = "/etc/kubernetes/pki")]
    cert_dir: PathBuf,
    /// API server address written into kubeconfigs (`https://host:port`).
    #[arg(long)]
    control_plane_endpoint: Option<String>,
    #[arg(long, default_value = "172.17.0.1")]
    apiserver_advertise_address: IpAddr,
    #[arg(long, default_value_t = 6443)]
    apiserver_bind_port: u16,
    /// Cluster name in kubeconfigs.
    #[arg(long, default_value = "kubernetes")]
    cluster_name: String,
}

impl Common {
    fn server(&self) -> String {
        self.control_plane_endpoint
            .clone()
            .unwrap_or_else(|| format!("https://{}:{}", self.apiserver_advertise_address, self.apiserver_bind_port))
    }
}

#[derive(Args)]
struct InitOpts {
    /// `phase certs all` / `phase kubeconfig all`; omit to run both.
    #[arg(num_args = 0..=3)]
    phase: Vec<String>,
    #[command(flatten)]
    common: Common,
    #[arg(long, default_value = "/etc/kubernetes")]
    kubeconfig_dir: PathBuf,
    /// Extra Subject Alternative Names (IPs or DNS names) for the API server certificate.
    #[arg(long, value_delimiter = ',')]
    apiserver_cert_extra_sans: Vec<String>,
    /// First IP of the Service range (the `kubernetes` Service ClusterIP).
    #[arg(long, default_value = "10.96.0.1")]
    service_cluster_ip: IpAddr,
    #[arg(long, default_value = "cluster.local")]
    service_dns_domain: String,
    /// Node name for the kubelet certificates.
    #[arg(long)]
    node_name: Option<String>,
    /// Node IPs in the kubelet serving certificate.
    #[arg(long, value_delimiter = ',')]
    node_ip: Vec<IpAddr>,
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_lowercase()).unwrap_or_else(|_| "localhost".into())
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Init(o) => {
            let phase: Vec<&str> = o.phase.iter().map(String::as_str).collect();
            let (do_certs, do_kc) = match phase.as_slice() {
                [] => (true, true),
                ["phase", "certs", ..] => (true, false),
                ["phase", "kubeconfig", ..] => (false, true),
                other => anyhow::bail!("unknown phase {other:?} (want: phase certs all | phase kubeconfig all)"),
            };
            let node = o.node_name.clone().unwrap_or_else(hostname);
            let co = certs::CertOptions {
                cert_dir: o.common.cert_dir.clone(),
                advertise_address: o.common.apiserver_advertise_address,
                service_cluster_ip: o.service_cluster_ip,
                dns_domain: o.service_dns_domain.clone(),
                extra_sans: o.apiserver_cert_extra_sans.clone(),
                node_name: node.clone(),
                node_ips: if o.node_ip.is_empty() { vec![o.common.apiserver_advertise_address] } else { o.node_ip.clone() },
            };
            let ca = if do_certs { certs::all(&co)? } else { certs::ensure_ca(&co.cert_dir)? };
            if do_kc {
                kubeconfig::all(&o.kubeconfig_dir, &ca, &o.common.server(), &o.common.cluster_name, &node)?;
            }
        }
        Cmd::NodeCredentials { node_name, node_ip, out, common } => {
            if !common.cert_dir.join("ca.key").exists() {
                anyhow::bail!("{}/ca.key not found: run `kubeadm init` first or pass --cert-dir", common.cert_dir.display());
            }
            let ca = certs::ensure_ca(&common.cert_dir)?;
            std::fs::create_dir_all(&out)?;
            let mut ips = node_ip.clone();
            ips.push("127.0.0.1".parse()?);
            ips.dedup();
            let (crt, key) = certs::issue(
                &ca,
                &certs::LeafSpec {
                    cn: &node_name,
                    orgs: &[],
                    dns: vec![node_name.clone(), "localhost".into()],
                    ips,
                    server: true,
                    client: false,
                },
            )?;
            std::fs::write(out.join("kubelet.crt"), crt)?;
            std::fs::write(out.join("kubelet.key"), key)?;
            std::fs::write(out.join("ca.crt"), &ca.pem)?;
            let kc = kubeconfig::user(&ca, &common.server(), &common.cluster_name, &format!("system:node:{node_name}"), &["system:nodes"])?;
            std::fs::write(out.join("kubelet.conf"), kc)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                for f in ["kubelet.key", "kubelet.conf"] {
                    std::fs::set_permissions(out.join(f), std::fs::Permissions::from_mode(0o600))?;
                }
            }
            println!(
                "[node-credentials] Wrote kubelet.crt, kubelet.key, kubelet.conf and ca.crt for node {node_name:?} to {}",
                out.display()
            );
        }
        Cmd::Kubeconfig { cmd: KubeconfigCmd::User { client_name, orgs, common } } => {
            if !common.cert_dir.join("ca.key").exists() {
                anyhow::bail!("{}/ca.key not found: run `kubeadm init` first or pass --cert-dir", common.cert_dir.display());
            }
            let ca = certs::ensure_ca(&common.cert_dir)?;
            let orgs: Vec<&str> = orgs.iter().map(String::as_str).collect();
            print!("{}", kubeconfig::user(&ca, &common.server(), &common.cluster_name, &client_name, &orgs)?);
        }
    }
    Ok(())
}
