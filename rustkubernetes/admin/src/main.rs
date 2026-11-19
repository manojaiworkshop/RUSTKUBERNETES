//! `rustkubeadmin`: a web console for rustkubernetes.
//!
//! * Sign in as the super admin from `RUSTKUBEADMIN_USER` / `RUSTKUBEADMIN_PASSWORD`,
//!   create users and give them console permissions and cluster access.
//! * Add machines (this host or over SSH); the offline installer copies every
//!   binary and container image to them — no internet needed on the machines.
//! * Create clusters from machines (one master, any number of workers), import
//!   existing ones, and watch nodes and pods live.
//!
//! The UI (`admin/web`, Next.js) is embedded in the binary at build time.

mod api;
mod auth;
mod bundle;
mod clusters;
mod events;
mod jobs;
mod machines;
mod state;
mod transport;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "rustkubeadmin", version, about = "Web console for rustkubernetes: clusters, machines, users and RBAC")]
struct Opts {
    /// Where to serve the console.
    #[arg(long, default_value = "127.0.0.1:8090")]
    listen: String,
    /// Console state: users, machines, cluster PKI, bundle, logs.
    #[arg(long, env = "RUSTKUBEADMIN_DATA")]
    data_dir: Option<PathBuf>,
    /// Directory with the rustkubernetes binaries packed into the offline bundle
    /// (default: the directory of this executable).
    #[arg(long, env = "RUSTKUBEADMIN_BINARIES")]
    binaries: Option<PathBuf>,
    /// Allow this origin in development (`npm run dev` on another port).
    #[arg(long, env = "RUSTKUBEADMIN_DEV_ORIGIN")]
    dev_origin: Option<String>,
    #[arg(short = 'v', long = "v", default_value_t = 1)]
    v: u8,
}

/// Everything the handlers share.
pub struct App {
    pub store: state::Store,
    pub sessions: auth::Sessions,
    pub hub: Arc<events::Hub>,
    pub jobs: Arc<jobs::Jobs>,
    pub bundle: bundle::Bundle,
    pub ssh: transport::SshPaths,
    pub super_user: String,
    pub super_password: String,
    pub runtimes: Mutex<HashMap<String, Arc<clusters::Runtime>>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let o = Opts::parse();
    let level = if o.v >= 2 { "debug" } else { "info" };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).init();
    k8s_client_go::util::cert::install_crypto_provider();

    let super_user = std::env::var("RUSTKUBEADMIN_USER").unwrap_or_else(|_| "admin".into());
    let super_password = match std::env::var("RUSTKUBEADMIN_PASSWORD") {
        Ok(p) if p.len() >= 8 => p,
        Ok(_) => anyhow::bail!("RUSTKUBEADMIN_PASSWORD must be at least 8 characters"),
        Err(_) => {
            anyhow::bail!("set RUSTKUBEADMIN_USER and RUSTKUBEADMIN_PASSWORD (the super admin's sign-in) before starting rustkubeadmin")
        }
    };
    let data = o
        .data_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".local/share/rustkubeadmin"));
    let binaries = match o.binaries.clone() {
        Some(b) => b,
        None => std::env::current_exe()?.parent().context("executable directory")?.to_path_buf(),
    };
    let store = state::Store::open(&data).with_context(|| format!("opening {}", data.display()))?;
    let hub = Arc::new(events::Hub::default());
    let app = Arc::new(App {
        jobs: jobs::Jobs::new(hub.clone(), data.join("jobs")),
        bundle: bundle::Bundle { binaries, dir: data.join("bundle"), build_lock: Default::default() },
        ssh: transport::SshPaths { dir: data.join("ssh") },
        store,
        sessions: auth::Sessions::default(),
        hub,
        super_user,
        super_password,
        runtimes: Mutex::new(HashMap::new()),
    });
    if let Err(e) = app.ssh.ensure_admin_key().await {
        tracing::warn!("SSH key: {e:#}");
    }
    let missing = app.bundle.missing_binaries();
    if !missing.is_empty() {
        tracing::warn!(dir = %app.bundle.binaries.display(), ?missing, "binaries for the offline bundle are missing (use --binaries)");
    }
    // Resume watches of clusters that were running.
    for c in app.store.read(|d| d.clusters.clone()) {
        if c.status == "running" {
            clusters::start_runtime(&app, &c.name);
        } else if c.status == "provisioning" || c.status == "deleting" {
            app.store.update_cluster(&c.name, |x| {
                x.status = "failed".into();
                x.message = "interrupted by a restart of rustkubeadmin".into();
            });
        }
    }
    app.store.write(|d| {
        for m in d.machines.iter_mut().filter(|m| m.status == "installing") {
            m.status = "failed".into();
            m.message = "interrupted by a restart of rustkubeadmin".into();
        }
    });
    machines::publish(&app);
    api::publish_users(&app);
    clusters::publish_clusters(&app);
    app.hub.publish("jobs", app.jobs.list());

    let listener = tokio::net::TcpListener::bind(&o.listen).await.with_context(|| format!("binding {}", o.listen))?;
    tracing::info!(super_admin = %app.super_user, data = %data.display(), binaries = %app.bundle.binaries.display(), "rustkubeadmin listening on http://{}", o.listen);
    axum::serve(listener, api::router(app.clone(), o.dev_origin.clone()))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
