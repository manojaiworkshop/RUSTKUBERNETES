//! Generates the etcd v3 gRPC client from etcd's own `.proto` files (copied
//! unchanged from go.etcd.io/etcd/api, Apache-2.0) with protox — pure Rust, no protoc.

fn main() {
    println!("cargo:rerun-if-changed=proto");
    let fds = protox::compile(["etcd/api/etcdserverpb/rpc.proto"], ["proto"]).expect("compiling etcd protos");
    tonic_prost_build::configure().build_server(false).build_transport(true).compile_fds(fds).expect("generating etcd client");
}
