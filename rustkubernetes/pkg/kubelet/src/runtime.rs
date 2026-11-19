//! The container runtime interface used by the kubelet — the subset of CRI's
//! `RuntimeService` and `ImageService` the kubelet needs
//! (`pkg/kubelet/container/runtime.go`, `cri-api`).
//!
//! A pod is a **sandbox** (holds the network namespace and IP) plus app/init
//! containers that join it. Implementations: [`crate::dockershim::DockerRuntime`].

use std::collections::BTreeMap;
use std::pin::Pin;

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use tokio::io::{AsyncRead, AsyncWrite};

/// Labels written on every runtime object (same keys as dockershim/CRI).
pub mod labels {
    pub const POD_UID: &str = "io.kubernetes.pod.uid";
    pub const POD_NAME: &str = "io.kubernetes.pod.name";
    pub const POD_NAMESPACE: &str = "io.kubernetes.pod.namespace";
    pub const CONTAINER_NAME: &str = "io.kubernetes.container.name";
    pub const CONTAINER_HASH: &str = "io.kubernetes.container.hash";
    pub const ATTEMPT: &str = "io.kubernetes.container.attempt";
    pub const TYPE: &str = "io.kubernetes.docker.type";
    pub const SANDBOX_ID: &str = "io.kubernetes.sandbox.id";
    /// Which kubelet (node) owns the object, so several kubelets can share one runtime.
    pub const NODE: &str = "io.rustkubernetes.node";
    pub const TYPE_SANDBOX: &str = "podsandbox";
    pub const TYPE_CONTAINER: &str = "container";
}

#[derive(Clone, Debug, Default)]
pub struct PortMapping {
    pub container_port: i32,
    pub host_port: i32,
    pub protocol: String,
    pub host_ip: String,
}

/// `PodSandboxConfig`
#[derive(Clone, Debug, Default)]
pub struct SandboxConfig {
    pub pod_uid: String,
    pub name: String,
    pub namespace: String,
    pub attempt: u32,
    pub hostname: String,
    pub host_network: bool,
    pub port_mappings: Vec<PortMapping>,
    pub labels: BTreeMap<String, String>,
    /// `None`: the runtime's default resolver (the host's, `dnsPolicy: Default`).
    pub dns: Option<DnsConfig>,
}

/// Pod resolver settings (`runtimeapi.DNSConfig`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DnsConfig {
    pub servers: Vec<String>,
    pub searches: Vec<String>,
    pub options: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SandboxState {
    Ready,
    NotReady,
}

#[derive(Clone, Debug)]
pub struct SandboxStatus {
    pub id: String,
    pub pod_uid: String,
    pub attempt: u32,
    pub state: SandboxState,
    pub ip: Option<String>,
    pub created_at: String,
    pub labels: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default)]
pub struct Mount {
    pub host_path: String,
    pub container_path: String,
    pub read_only: bool,
}

/// `ContainerConfig`
#[derive(Clone, Debug, Default)]
pub struct ContainerConfig {
    pub name: String,
    pub attempt: u32,
    pub image: String,
    /// Overrides the image ENTRYPOINT when non-empty.
    pub command: Vec<String>,
    /// Overrides the image CMD when non-empty.
    pub args: Vec<String>,
    pub working_dir: Option<String>,
    pub env: Vec<(String, String)>,
    pub mounts: Vec<Mount>,
    pub labels: BTreeMap<String, String>,
    pub tty: bool,
    pub stdin: bool,
    pub stdin_once: bool,
    pub cpu_limit_milli: Option<i64>,
    pub memory_limit: Option<i64>,
    pub run_as_user: Option<i64>,
    pub run_as_group: Option<i64>,
    pub privileged: bool,
    pub read_only_rootfs: bool,
    pub cap_add: Vec<String>,
    pub cap_drop: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerState {
    Created,
    Running,
    Exited,
    Unknown,
}

/// `ContainerStatus`
#[derive(Clone, Debug)]
pub struct ContainerStatus {
    pub id: String,
    pub pod_uid: String,
    pub name: String,
    pub attempt: u32,
    pub state: ContainerState,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub exit_code: i32,
    pub reason: Option<String>,
    pub message: Option<String>,
    pub image: String,
    pub image_ref: String,
    pub labels: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default)]
pub struct LogOptions {
    pub follow: bool,
    pub tail_lines: Option<i64>,
    pub timestamps: bool,
    pub since_seconds: Option<i64>,
}

/// A bidirectional exec stream.
pub trait ExecIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> ExecIo for T {}

pub struct ExecSession {
    pub exec_id: String,
    pub io: Box<dyn ExecIo>,
    /// When false, output is multiplexed (see `Demuxer`).
    pub tty: bool,
}

pub type LogStream = Pin<Box<dyn Stream<Item = Result<(u8, Bytes)>> + Send>>;

/// `RuntimeReady` / `NetworkReady` (CRI `RuntimeCondition`).
pub const RUNTIME_READY: &str = "RuntimeReady";
pub const NETWORK_READY: &str = "NetworkReady";

/// CRI `RuntimeCondition`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeCondition {
    pub type_: String,
    pub status: bool,
    pub reason: String,
    pub message: String,
}

impl RuntimeCondition {
    pub fn ok(type_: &str) -> Self {
        Self { type_: type_.into(), status: true, reason: String::new(), message: String::new() }
    }

    pub fn failed(type_: &str, reason: &str, message: String) -> Self {
        Self { type_: type_.into(), status: false, reason: reason.into(), message }
    }
}

/// `kubecontainer.RuntimeCondition.String()`
impl std::fmt::Display for RuntimeCondition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}={} reason:{} message:{}", self.type_, self.status, self.reason, self.message)
    }
}

/// CPU and memory of one container (`ContainerStats`, CRI `ContainerStats`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContainerStats {
    /// When the sample was taken (milliseconds since the epoch).
    pub timestamp_ms: i64,
    /// Cumulative CPU time in nanoseconds.
    pub cpu_usage_ns: u64,
    /// Memory usage minus inactive file pages (`working_set`).
    pub memory_working_set_bytes: u64,
}

#[async_trait]
pub trait Runtime: Send + Sync {
    /// e.g. `docker://29.0.1`
    async fn version(&self) -> Result<String>;

    /// CRI `Status`: the `RuntimeReady` and `NetworkReady` conditions.
    async fn status(&self) -> Result<Vec<RuntimeCondition>> {
        Ok(vec![RuntimeCondition::ok(RUNTIME_READY), RuntimeCondition::ok(NETWORK_READY)])
    }

    /// Creates a missing host directory (or empty file) for a `DirectoryOrCreate` /
    /// `FileOrCreate` hostPath volume the kubelet itself may not create.
    async fn ensure_host_path(&self, path: &std::path::Path, _dir: bool) -> Result<()> {
        Err(anyhow::anyhow!("cannot create {}: permission denied", path.display()))
    }

    /// CRI `UpdateRuntimeConfig`: the node's pod CIDR(s), comma separated.
    async fn update_runtime_config(&self, _pod_cidr: &str) -> Result<()> {
        Ok(())
    }

    async fn run_pod_sandbox(&self, cfg: &SandboxConfig) -> Result<String>;
    async fn stop_pod_sandbox(&self, id: &str) -> Result<()>;
    async fn remove_pod_sandbox(&self, id: &str) -> Result<()>;
    async fn list_pod_sandboxes(&self) -> Result<Vec<SandboxStatus>>;

    async fn create_container(&self, sandbox: &SandboxStatus, cfg: &ContainerConfig) -> Result<String>;
    async fn start_container(&self, id: &str) -> Result<()>;
    async fn stop_container(&self, id: &str, timeout_secs: i64) -> Result<()>;
    async fn remove_container(&self, id: &str) -> Result<()>;
    async fn list_containers(&self) -> Result<Vec<ContainerStatus>>;

    async fn image_present(&self, image: &str) -> Result<bool>;
    async fn pull_image(&self, image: &str) -> Result<()>;
    /// `(names, size)` of local images (reported in node status).
    async fn list_images(&self) -> Result<Vec<(Vec<String>, i64)>>;

    /// Runs a command to completion: `(exit code, stdout, stderr)`.
    async fn exec_sync(&self, id: &str, cmd: &[String], timeout_secs: u64) -> Result<(i32, Vec<u8>, Vec<u8>)>;
    async fn exec_stream(&self, id: &str, cmd: &[String], tty: bool, stdin: bool) -> Result<ExecSession>;
    async fn exec_resize(&self, exec_id: &str, width: u16, height: u16) -> Result<()>;
    /// Exit code of a finished exec (None while running).
    async fn exec_exit_code(&self, exec_id: &str) -> Result<Option<i32>>;

    async fn logs(&self, id: &str, opts: &LogOptions) -> Result<LogStream>;

    /// CRI `ContainerStats` for a running container.
    async fn container_stats(&self, _id: &str) -> Result<ContainerStats> {
        Err(anyhow::anyhow!("container stats are not supported by this runtime"))
    }

    /// CRI `Attach`: the container's own stdio (`exec_id` of the session is the container id).
    async fn attach(&self, _id: &str, _stdin: bool, _tty: bool) -> Result<ExecSession> {
        Err(anyhow::anyhow!("attach is not supported by this runtime"))
    }

    /// Resizes the TTY of an attached container.
    async fn resize_container(&self, _id: &str, _width: u16, _height: u16) -> Result<()> {
        Ok(())
    }

    /// CRI `PortForward`: copies `stream` to and from `localhost:port` inside the
    /// sandbox's network namespace until either side finishes.
    async fn port_forward(&self, _sandbox_id: &str, _port: u16, _stream: tokio::io::DuplexStream) -> Result<()> {
        Err(anyhow::anyhow!("port forwarding is not supported by this runtime"))
    }
}
