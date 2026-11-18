//! `kube-apiserver` entry point (`cmd/kube-apiserver/apiserver.go`).

mod app;

fn main() -> anyhow::Result<()> {
    let opts = <app::options::ServerRunOptions as clap::Parser>::parse();
    app::init_logging(opts.v);
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(app::run(opts))
}
