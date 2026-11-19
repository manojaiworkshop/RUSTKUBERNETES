//! `kubeadm` (subset): generates the PKI and kubeconfig files a secure cluster
//! needs, in kubeadm's layout (`<dir>/pki/*`, `<dir>/*.conf`).
//!
//! ```text
//! kubeadm init phase certs all       --cert-dir DIR/pki ...
//! kubeadm init phase kubeconfig all  --cert-dir DIR/pki --kubeconfig-dir DIR ...
//! kubeadm init                       both of the above
//! kubeadm kubeconfig user --client-name jane [--org dev]   (kubeconfig on stdout)
//! kubeadm init phase bootstrap-token [--kubeconfig admin.conf]
//! kubeadm token create|list|delete|generate [--kubeconfig admin.conf]
//! kubeadm certs check-expiration | renew <name>|all
//! kubeadm join <endpoint> --token T --discovery-token-ca-cert-hash sha256:H
//! ```
//!
//! Static pods, etcd and add-on installation are done by `hack/local-up-cluster.sh`.

mod app;

use std::net::IpAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use app::phases::{bootstraptoken, certs, kubeconfig, renewal};

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
    /// Manage bootstrap tokens.
    Token {
        #[command(subcommand)]
        cmd: TokenCmd,
        /// The kubeconfig file to use when talking to the cluster.
        #[arg(long, global = true, default_value = "/etc/kubernetes/admin.conf")]
        kubeconfig: PathBuf,
    },
    /// Commands related to handling kubernetes certificates.
    Certs {
        #[command(subcommand)]
        cmd: CertsCmd,
    },
    /// Run this on any machine you wish to join an existing cluster: discovers the
    /// cluster with a bootstrap token and writes the kubelet's TLS bootstrap
    /// credentials (`bootstrap-kubelet.conf`, `pki/ca.crt`).
    Join {
        /// API server endpoint (`host:port`).
        endpoint: String,
        /// Use this token for both discovery-token and tls-bootstrap-token.
        #[arg(long)]
        token: String,
        /// For token-based discovery, validate that the root CA public key matches this hash (format: "<type>:<value>").
        #[arg(long, value_delimiter = ',')]
        discovery_token_ca_cert_hash: Vec<String>,
        /// For token-based discovery, allow joining without --discovery-token-ca-cert-hash pinning.
        #[arg(long)]
        discovery_token_unsafe_skip_ca_verification: bool,
        /// Where kubeconfig files are written.
        #[arg(long, default_value = "/etc/kubernetes")]
        kubeconfig_dir: PathBuf,
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

#[derive(Subcommand)]
enum TokenCmd {
    /// Create bootstrap tokens on the server.
    Create {
        /// The token to create (generated when omitted).
        token: Option<String>,
        /// The duration before the token is automatically deleted (e.g. 1s, 2m, 3h). If set to '0', the token will never expire.
        #[arg(long, default_value = "24h0m0s", value_parser = k8s_apimachinery::duration::parse_go_duration)]
        ttl: std::time::Duration,
        /// Describes the ways in which this token can be used.
        #[arg(long, value_delimiter = ',', default_values_t = bootstraptoken::DEFAULT_USAGES.iter().map(|s| s.to_string()).collect::<Vec<_>>())]
        usages: Vec<String>,
        /// Extra groups that this token will authenticate as when used for authentication.
        #[arg(long, value_delimiter = ',', default_values_t = vec![bootstraptoken::NODE_BOOTSTRAP_TOKEN_AUTH_GROUP.to_string()])]
        groups: Vec<String>,
        /// A human friendly description of how this token is used.
        #[arg(long, default_value = "")]
        description: String,
        /// Instead of printing only the token, print the full 'kubeadm join' flag needed to join the cluster using the token.
        #[arg(long)]
        print_join_command: bool,
    },
    /// List bootstrap tokens on the server.
    List,
    /// Delete bootstrap tokens on the server.
    Delete { tokens: Vec<String> },
    /// Generate and print a bootstrap token, but do not create it on the server.
    Generate,
}

#[derive(Subcommand)]
enum CertsCmd {
    /// Check certificates expiration for a Kubernetes cluster.
    CheckExpiration {
        #[arg(long, default_value = "/etc/kubernetes/pki")]
        cert_dir: PathBuf,
        #[arg(long, default_value = "/etc/kubernetes")]
        kubeconfig_dir: PathBuf,
    },
    /// Renew certificates for a Kubernetes cluster (`all` or one certificate's name).
    Renew {
        name: String,
        #[arg(long, default_value = "/etc/kubernetes/pki")]
        cert_dir: PathBuf,
        #[arg(long, default_value = "/etc/kubernetes")]
        kubeconfig_dir: PathBuf,
    },
}

/// A client from a kubeconfig file, plus its server and CA.
fn admin_client(path: &std::path::Path) -> anyhow::Result<(k8s_client_go::Client, String, Vec<u8>)> {
    k8s_client_go::util::cert::install_crypto_provider();
    let cfg = k8s_client_go::tools::clientcmd::load_config(path, None)
        .map_err(|e| anyhow::anyhow!("failed to load admin kubeconfig: open {}: {e}", path.display()))?;
    let (server, ca) = (cfg.server.clone(), cfg.tls.ca_data.clone().unwrap_or_default());
    Ok((k8s_client_go::Client::new(cfg)?, server, ca))
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime")
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
    /// The kubeconfig file to use when talking to the cluster (`phase bootstrap-token`).
    #[arg(long, default_value = "/etc/kubernetes/admin.conf")]
    kubeconfig: PathBuf,
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_lowercase()).unwrap_or_else(|_| "localhost".into())
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Init(o) => {
            let phase: Vec<&str> = o.phase.iter().map(String::as_str).collect();
            if let ["phase", "bootstrap-token", ..] = phase.as_slice() {
                let (client, server, ca) = admin_client(&o.kubeconfig)?;
                return runtime().block_on(bootstraptoken::init_phase(&client, &server, &String::from_utf8(ca)?));
            }
            let (do_certs, do_kc) = match phase.as_slice() {
                [] => (true, true),
                ["phase", "certs", ..] => (true, false),
                ["phase", "kubeconfig", ..] => (false, true),
                other => anyhow::bail!("unknown phase {other:?} (want: phase certs all | phase kubeconfig all | phase bootstrap-token)"),
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
        Cmd::Token { cmd, kubeconfig } => {
            match cmd {
                TokenCmd::Generate => println!("{}", bootstraptoken::generate_token()),
                TokenCmd::Create { token, ttl, usages, groups, description, print_join_command } => {
                    let (client, server, ca) = admin_client(&kubeconfig)?;
                    let token = token.unwrap_or_else(bootstraptoken::generate_token);
                    runtime().block_on(bootstraptoken::create(&client, &token, ttl, &usages, &groups, &description))?;
                    if print_join_command {
                        println!("{}", bootstraptoken::join_command(&server, &token, &ca)?);
                    } else {
                        println!("{token}");
                    }
                }
                TokenCmd::List => {
                    let (client, _, _) = admin_client(&kubeconfig)?;
                    print!("{}", runtime().block_on(bootstraptoken::list(&client))?);
                }
                TokenCmd::Delete { tokens } => {
                    if tokens.is_empty() {
                        anyhow::bail!("missing argument; 'token delete' is missing token of form \"[a-z0-9]{{6}}\" or \"[a-z0-9]{{6}}\\.[a-z0-9]{{16}}\"");
                    }
                    let (client, _, _) = admin_client(&kubeconfig)?;
                    for t in tokens {
                        runtime().block_on(bootstraptoken::delete(&client, &t))?;
                    }
                }
            }
        }
        Cmd::Certs { cmd: CertsCmd::CheckExpiration { cert_dir, kubeconfig_dir } } => {
            print!("{}", renewal::check_expiration(&renewal::Dirs { cert_dir, kubeconfig_dir })?);
        }
        Cmd::Certs { cmd: CertsCmd::Renew { name, cert_dir, kubeconfig_dir } } => {
            renewal::renew(&renewal::Dirs { cert_dir, kubeconfig_dir }, &name)?;
        }
        Cmd::Join { endpoint, token, discovery_token_ca_cert_hash, discovery_token_unsafe_skip_ca_verification, kubeconfig_dir } => {
            k8s_client_go::util::cert::install_crypto_provider();
            let (server, ca) = runtime().block_on(bootstraptoken::discover(
                &endpoint,
                &token,
                &discovery_token_ca_cert_hash,
                discovery_token_unsafe_skip_ca_verification,
            ))?;
            std::fs::create_dir_all(kubeconfig_dir.join("pki"))?;
            std::fs::write(kubeconfig_dir.join("pki/ca.crt"), &ca)?;
            let boot = kubeconfig_dir.join("bootstrap-kubelet.conf");
            std::fs::write(&boot, bootstraptoken::bootstrap_kubeconfig(&server, &ca, &token))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&boot, std::fs::Permissions::from_mode(0o600))?;
            }
            println!("[kubelet-start] Writing TLS bootstrap credentials to {}", boot.display());
            println!(
                "\nThis node is ready to join. Start the kubelet with:\n  --bootstrap-kubeconfig={} --kubeconfig={} --rotate-certificates\nIt will request a client certificate and register the node.",
                boot.display(),
                kubeconfig_dir.join("kubelet.conf").display()
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
