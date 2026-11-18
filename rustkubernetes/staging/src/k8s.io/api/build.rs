//! Compiles the upstream Kubernetes `generated.proto` files into a
//! `FileDescriptorSet` at build time (pure Rust, no `protoc` needed).
//! The descriptor set is embedded and used for dynamic protobuf decoding.

use prost::Message;

fn main() {
    let files = [
        "k8s.io/api/core/v1/generated.proto",
        "k8s.io/api/apps/v1/generated.proto",
        "k8s.io/api/batch/v1/generated.proto",
        "k8s.io/api/coordination/v1/generated.proto",
        "k8s.io/api/discovery/v1/generated.proto",
        "k8s.io/api/autoscaling/v1/generated.proto",
        "k8s.io/api/policy/v1/generated.proto",
        "k8s.io/api/scheduling/v1/generated.proto",
        "k8s.io/api/rbac/v1/generated.proto",
        "k8s.io/api/authorization/v1/generated.proto",
        "k8s.io/api/authentication/v1/generated.proto",
        "k8s.io/apimachinery/pkg/apis/meta/v1/generated.proto",
        "k8s.io/apimachinery/pkg/runtime/generated.proto",
    ];
    println!("cargo:rerun-if-changed=proto");
    let fds = protox::compile(files, ["proto"]).expect("compiling Kubernetes .proto files");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("k8s_descriptors.bin");
    std::fs::write(out, fds.encode_to_vec()).unwrap();
}
