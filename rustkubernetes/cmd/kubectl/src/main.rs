//! `rkubectl`: a small kubectl written on top of the Rust client-go.
//!
//! Supported: get (table/wide/yaml/json/name, -w), apply (server-side),
//! create, delete, scale, api-resources, version, cluster-info.
//! The official `kubectl` also works against rustkubernetes.

mod discovery;
mod printer;

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use futures::StreamExt;
use k8s_apimachinery::unstructured::ObjectExt;
use k8s_client_go::api::{ApiResource, DynamicApi};
use k8s_client_go::tools::clientcmd;
use k8s_client_go::{Client, Config, ListParams, PatchType};
use serde_json::{json, Value};

use discovery::Discovery;

#[derive(Parser)]
#[command(name = "rkubectl", version, about = "rkubectl controls the rustkubernetes cluster manager.")]
struct Cli {
    /// Path to the kubeconfig file.
    #[arg(long, global = true, env = "KUBECONFIG")]
    kubeconfig: Option<PathBuf>,
    /// The address of the Kubernetes API server (overrides kubeconfig).
    #[arg(short = 's', long, global = true)]
    server: Option<String>,
    /// Namespace for this request.
    #[arg(short = 'n', long, global = true)]
    namespace: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Display one or many resources.
    Get {
        /// TYPE[,TYPE...] or TYPE/NAME, followed by optional names.
        args: Vec<String>,
        #[arg(short = 'A', long)]
        all_namespaces: bool,
        #[arg(short = 'l', long)]
        selector: Option<String>,
        #[arg(long)]
        field_selector: Option<String>,
        /// Output format: wide, yaml, json, name.
        #[arg(short = 'o', long)]
        output: Option<String>,
        /// Watch for changes after listing.
        #[arg(short = 'w', long)]
        watch: bool,
    },
    /// Apply a configuration to a resource by file (server-side apply).
    Apply {
        #[arg(short = 'f', long)]
        filename: PathBuf,
    },
    /// Create a resource from a file.
    Create {
        #[arg(short = 'f', long)]
        filename: PathBuf,
    },
    /// Delete resources by TYPE NAME..., TYPE/NAME, or file.
    Delete {
        args: Vec<String>,
        #[arg(short = 'f', long)]
        filename: Option<PathBuf>,
        #[arg(long)]
        grace_period: Option<i64>,
    },
    /// Set a new size for a deployment, replica set or stateful set.
    Scale {
        /// TYPE/NAME
        target: String,
        #[arg(long)]
        replicas: i64,
    },
    /// Print the supported API resources on the server.
    ApiResources,
    /// Print the client and server version information.
    Version,
    /// Display addresses of the control plane.
    ClusterInfo,
}

struct Ctx {
    client: Client,
    namespace: String,
    disco: Discovery,
}

impl Ctx {
    fn api(&self, r: &ApiResource, ns: Option<&str>) -> DynamicApi {
        DynamicApi::new(self.client.clone(), r.clone(), ns.or(Some(&self.namespace)))
    }
}

fn read_docs(path: &PathBuf) -> Result<Vec<Value>> {
    let raw = if path.as_os_str() == "-" {
        std::io::read_to_string(std::io::stdin())?
    } else {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    };
    let mut out = vec![];
    for doc in serde_yaml::Deserializer::from_str(&raw) {
        let v: Value = serde::Deserialize::deserialize(doc).context("parsing YAML")?;
        if v.is_null() {
            continue;
        }
        if v.kind().ends_with("List") {
            out.extend(v["items"].as_array().cloned().unwrap_or_default());
        } else {
            out.push(v);
        }
    }
    Ok(out)
}

/// `deployment.apps/web`
fn display_name(r: &ApiResource, name: &str) -> String {
    if r.group.is_empty() {
        format!("{}/{name}", r.kind.to_lowercase())
    } else {
        format!("{}.{}/{name}", r.kind.to_lowercase(), r.group)
    }
}

#[tokio::main]
async fn main() {
    // Behave like a normal CLI when piped into `head` (no panic on EPIPE).
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    if let Err(e) = run(Cli::parse()).await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let kc_path = cli.kubeconfig.clone().or_else(clientcmd::default_kubeconfig_path);
    let config = match (&cli.server, &kc_path) {
        (Some(s), _) => Config::new(s),
        (None, Some(p)) => clientcmd::load_config(p, None)?,
        (None, None) => Config::new("http://127.0.0.1:6443"),
    };
    let namespace =
        cli.namespace.clone().or_else(|| kc_path.as_ref().map(|p| clientcmd::current_namespace(p))).unwrap_or_else(|| "default".into());
    let client = Client::new(config)?;
    let ctx = Ctx { disco: Discovery::new(client.clone()), client, namespace };

    match cli.cmd {
        Cmd::Version => {
            println!("Client Version: rkubectl v{}", env!("CARGO_PKG_VERSION"));
            let v = ctx.client.get_json("/version").await?;
            println!("Server Version: {}", v["gitVersion"].as_str().unwrap_or("?"));
        }
        Cmd::ClusterInfo => {
            println!("Kubernetes control plane is running at {}", ctx.client.config().server);
        }
        Cmd::ApiResources => {
            let mut rows = vec![vec!["NAME".into(), "SHORTNAMES".into(), "APIVERSION".into(), "NAMESPACED".into(), "KIND".into()]];
            for e in ctx.disco.all().await? {
                let av = if e.resource.group.is_empty() {
                    e.resource.version.clone()
                } else {
                    format!("{}/{}", e.resource.group, e.resource.version)
                };
                rows.push(vec![
                    e.resource.plural.clone(),
                    e.short_names.join(","),
                    av,
                    e.resource.namespaced.to_string(),
                    e.resource.kind.clone(),
                ]);
            }
            printer::print_rows(&rows);
        }
        Cmd::Get { args, all_namespaces, selector, field_selector, output, watch } => {
            get(&ctx, args, all_namespaces, selector, field_selector, output, watch).await?;
        }
        Cmd::Apply { filename } => {
            let mut failed = false;
            for doc in read_docs(&filename)? {
                let res: Result<()> = async {
                    let r = ctx.disco.for_object(&doc).await?;
                    let ns = doc.namespace().to_string();
                    let api = ctx.api(&r, Some(if ns.is_empty() { &ctx.namespace } else { &ns }));
                    let name = doc.name().to_string();
                    let existed = api.get(&name).await.is_ok();
                    api.patch(&name, PatchType::Apply, &doc).await?;
                    println!("{} {}", display_name(&r, &name), if existed { "configured" } else { "created" });
                    Ok(())
                }
                .await;
                if let Err(e) = res {
                    eprintln!("Error from server: {e:#}");
                    failed = true;
                }
            }
            if failed {
                std::process::exit(1);
            }
        }
        Cmd::Create { filename } => {
            let mut failed = false;
            for doc in read_docs(&filename)? {
                let res: Result<()> = async {
                    let r = ctx.disco.for_object(&doc).await?;
                    let ns = doc.namespace().to_string();
                    let api = ctx.api(&r, Some(if ns.is_empty() { &ctx.namespace } else { &ns }));
                    let created = api.create(&doc).await?;
                    println!("{} created", display_name(&r, created.name()));
                    Ok(())
                }
                .await;
                if let Err(e) = res {
                    eprintln!("Error from server: {e:#}");
                    failed = true;
                }
            }
            if failed {
                std::process::exit(1);
            }
        }
        Cmd::Delete { args, filename, grace_period } => {
            let mut targets: Vec<(ApiResource, String, Option<String>)> = vec![];
            if let Some(f) = filename {
                for doc in read_docs(&f)? {
                    let ns = Some(doc.namespace().to_string()).filter(|s| !s.is_empty());
                    targets.push((ctx.disco.for_object(&doc).await?, doc.name().to_string(), ns));
                }
            }
            for (r, name) in resolve_args(&ctx, &args).await? {
                let Some(name) = name else { bail!("resource(s) were provided, but no name was specified") };
                targets.push((r, name, None));
            }
            for (r, name, ns) in targets {
                ctx.api(&r, ns.as_deref()).delete(&name, grace_period).await?;
                println!("{} deleted", display_name(&r, &name).replacen('/', " \"", 1) + "\"");
            }
        }
        Cmd::Scale { target, replicas } => {
            let (r, name) = resolve_args(&ctx, &[target]).await?.into_iter().next().context("TYPE/NAME required")?;
            let name = name.context("TYPE/NAME required")?;
            ctx.api(&r, None).patch_sub(&name, Some("scale"), PatchType::Merge, &json!({"spec": {"replicas": replicas}})).await?;
            println!("{} scaled", display_name(&r, &name));
        }
    }
    Ok(())
}

/// `pods`, `pods foo bar`, `deploy/web`, `po,svc`.
async fn resolve_args(ctx: &Ctx, args: &[String]) -> Result<Vec<(ApiResource, Option<String>)>> {
    let Some(first) = args.first() else { bail!("you must specify the type of resource") };
    let mut out = vec![];
    if args.iter().all(|a| a.contains('/')) {
        for a in args {
            let (t, n) = a.split_once('/').unwrap();
            out.push((ctx.disco.resolve(t).await?, Some(n.to_string())));
        }
        return Ok(out);
    }
    let types: Vec<ApiResource> =
        futures::future::try_join_all(first.split(',').map(|t| ctx.disco.resolve_many(t))).await?.into_iter().flatten().collect();
    let names = &args[1..];
    for t in types {
        if names.is_empty() {
            out.push((t, None));
        } else {
            for n in names {
                out.push((t.clone(), Some(n.clone())));
            }
        }
    }
    Ok(out)
}

async fn get(
    ctx: &Ctx,
    args: Vec<String>,
    all_ns: bool,
    selector: Option<String>,
    field_selector: Option<String>,
    output: Option<String>,
    watch: bool,
) -> Result<()> {
    let targets = resolve_args(ctx, &args).await?;
    let multi = targets.len() > 1;
    let fmt = output.as_deref().unwrap_or("");
    let mut found_any = false;
    for (i, (r, name)) in targets.iter().enumerate() {
        let ns = if all_ns && r.namespaced { None } else { Some(ctx.namespace.as_str()) };
        let api = DynamicApi::new(ctx.client.clone(), r.clone(), ns);
        let lp = ListParams { label_selector: selector.clone(), field_selector: field_selector.clone(), ..Default::default() };
        let objects: Vec<Value> = match name {
            Some(n) => vec![api.get(n).await?],
            None => api.list(&lp).await?["items"].as_array().cloned().unwrap_or_default(),
        };
        found_any |= !objects.is_empty();
        match fmt {
            "yaml" | "json" => printer::print_objects(&objects, fmt, name.is_some())?,
            "name" => objects.iter().for_each(|o| println!("{}", display_name(r, o.name()))),
            _ => {
                if objects.is_empty() {
                    continue;
                }
                if i > 0 {
                    println!();
                }
                let path = r.collection_path(ns);
                let table = printer::fetch_table(&ctx.client, &path, name.as_deref(), &lp).await?;
                let prefix = multi.then(|| display_name(r, "").trim_end_matches('/').to_string());
                printer::print_table(&table, fmt == "wide", all_ns && r.namespaced, prefix.as_deref(), true);
            }
        }
        if watch {
            let mut lp = lp.clone();
            if let Some(n) = name {
                lp.field_selector = Some(format!("metadata.name={n}"));
            }
            let list = api.list(&lp).await?;
            lp.resource_version = list["metadata"]["resourceVersion"].as_str().map(String::from);
            let mut stream = Box::pin(api.watch(&lp).await?);
            while let Some(ev) = stream.next().await {
                let ev = ev?;
                if ev.type_ == k8s_apimachinery::watch::EventType::Bookmark {
                    continue;
                }
                let o = &ev.object;
                println!(
                    "{:<9} {}",
                    ev.type_.as_str(),
                    if o.namespace().is_empty() { o.name().to_string() } else { format!("{}/{}", o.namespace(), o.name()) }
                );
            }
        }
    }
    if !found_any && fmt.is_empty() && !watch {
        if all_ns {
            eprintln!("No resources found");
        } else {
            eprintln!("No resources found in {} namespace.", ctx.namespace);
        }
    }
    Ok(())
}
