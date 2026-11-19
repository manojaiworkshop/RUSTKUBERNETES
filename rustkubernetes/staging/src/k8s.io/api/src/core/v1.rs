//! Port of `k8s.io/api/core/v1` (commonly used subset).
//!
//! Fields that are not modelled are still preserved by the API server, because
//! it stores objects as untyped JSON. Typed structs ignore unknown fields.

use std::collections::BTreeMap;

use k8s_apimachinery::meta::ObjectMeta;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_with::skip_serializing_none;

use crate::{impl_resource, IntOrString};

// ---------------------------------------------------------------------------
// Namespace
// ---------------------------------------------------------------------------

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Namespace {
    pub api_version: Option<String>,
    pub kind: Option<String>,
    #[serde(default)]
    pub metadata: ObjectMeta,
    pub spec: Option<NamespaceSpec>,
    pub status: Option<NamespaceStatus>,
}
impl_resource!(Namespace, "", "v1", "Namespace", "namespaces", false);

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceSpec {
    pub finalizers: Option<Vec<String>>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceStatus {
    /// `Active` or `Terminating`
    pub phase: Option<String>,
}

// ---------------------------------------------------------------------------
// Pod
// ---------------------------------------------------------------------------

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pod {
    pub api_version: Option<String>,
    pub kind: Option<String>,
    #[serde(default)]
    pub metadata: ObjectMeta,
    pub spec: Option<PodSpec>,
    pub status: Option<PodStatus>,
}
impl_resource!(Pod, "", "v1", "Pod", "pods", true);

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PodSpec {
    #[serde(default)]
    pub containers: Vec<Container>,
    pub init_containers: Option<Vec<Container>>,
    pub volumes: Option<Vec<Volume>>,
    /// `Always`, `OnFailure`, `Never`
    pub restart_policy: Option<String>,
    pub termination_grace_period_seconds: Option<i64>,
    pub active_deadline_seconds: Option<i64>,
    pub dns_policy: Option<String>,
    pub node_selector: Option<BTreeMap<String, String>>,
    pub service_account_name: Option<String>,
    pub automount_service_account_token: Option<bool>,
    pub node_name: Option<String>,
    pub host_network: Option<bool>,
    #[serde(rename = "hostPID")]
    pub host_pid: Option<bool>,
    #[serde(rename = "hostIPC")]
    pub host_ipc: Option<bool>,
    pub hostname: Option<String>,
    pub subdomain: Option<String>,
    pub affinity: Option<Value>,
    pub scheduler_name: Option<String>,
    pub tolerations: Option<Vec<Toleration>>,
    pub priority_class_name: Option<String>,
    pub priority: Option<i32>,
    pub security_context: Option<Value>,
    pub image_pull_secrets: Option<Vec<LocalObjectReference>>,
    pub runtime_class_name: Option<String>,
    pub enable_service_links: Option<bool>,
    pub preemption_policy: Option<String>,
    pub topology_spread_constraints: Option<Vec<Value>>,
    /// Fields not modelled above (kept so controllers copy templates losslessly).
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Container {
    pub name: String,
    pub image: Option<String>,
    pub command: Option<Vec<String>>,
    pub args: Option<Vec<String>>,
    pub working_dir: Option<String>,
    pub ports: Option<Vec<ContainerPort>>,
    pub env: Option<Vec<EnvVar>>,
    pub env_from: Option<Vec<Value>>,
    pub resources: Option<ResourceRequirements>,
    pub volume_mounts: Option<Vec<VolumeMount>>,
    pub liveness_probe: Option<Probe>,
    pub readiness_probe: Option<Probe>,
    pub startup_probe: Option<Probe>,
    pub lifecycle: Option<Value>,
    pub termination_message_path: Option<String>,
    pub termination_message_policy: Option<String>,
    /// `Always`, `IfNotPresent`, `Never`
    pub image_pull_policy: Option<String>,
    pub security_context: Option<Value>,
    /// For init containers: `Always` makes it a sidecar.
    pub restart_policy: Option<String>,
    pub stdin: Option<bool>,
    pub stdin_once: Option<bool>,
    pub tty: Option<bool>,
    /// Fields not modelled above.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerPort {
    pub name: Option<String>,
    pub host_port: Option<i32>,
    pub container_port: i32,
    /// `TCP`, `UDP`, `SCTP`
    pub protocol: Option<String>,
    #[serde(rename = "hostIP")]
    pub host_ip: Option<String>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvVar {
    pub name: String,
    pub value: Option<String>,
    pub value_from: Option<Value>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceRequirements {
    pub limits: Option<BTreeMap<String, String>>,
    pub requests: Option<BTreeMap<String, String>>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeMount {
    pub name: String,
    pub mount_path: String,
    pub read_only: Option<bool>,
    pub sub_path: Option<String>,
}

/// A pod volume. The source (`emptyDir`, `configMap`, `hostPath`, ...) is kept untyped.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Volume {
    pub name: String,
    #[serde(flatten)]
    pub source: BTreeMap<String, Value>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Probe {
    pub exec: Option<ExecAction>,
    pub http_get: Option<HttpGetAction>,
    pub tcp_socket: Option<TcpSocketAction>,
    pub grpc: Option<Value>,
    pub initial_delay_seconds: Option<i32>,
    pub timeout_seconds: Option<i32>,
    pub period_seconds: Option<i32>,
    pub success_threshold: Option<i32>,
    pub failure_threshold: Option<i32>,
    pub termination_grace_period_seconds: Option<i64>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecAction {
    pub command: Option<Vec<String>>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpGetAction {
    pub path: Option<String>,
    pub port: IntOrString,
    pub host: Option<String>,
    pub scheme: Option<String>,
    pub http_headers: Option<Vec<HttpHeader>>,
}

/// `HTTPHeader`: a custom header for HTTP probes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HttpHeader {
    pub name: String,
    pub value: String,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TcpSocketAction {
    pub port: IntOrString,
    pub host: Option<String>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Toleration {
    pub key: Option<String>,
    /// `Exists` or `Equal`
    pub operator: Option<String>,
    pub value: Option<String>,
    /// `NoSchedule`, `PreferNoSchedule`, `NoExecute`
    pub effect: Option<String>,
    pub toleration_seconds: Option<i64>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalObjectReference {
    pub name: Option<String>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PodStatus {
    /// `Pending`, `Running`, `Succeeded`, `Failed`, `Unknown`
    pub phase: Option<String>,
    pub conditions: Option<Vec<PodCondition>>,
    pub message: Option<String>,
    pub reason: Option<String>,
    pub nominated_node_name: Option<String>,
    #[serde(rename = "hostIP")]
    pub host_ip: Option<String>,
    #[serde(rename = "podIP")]
    pub pod_ip: Option<String>,
    #[serde(rename = "podIPs")]
    pub pod_ips: Option<Vec<PodIp>>,
    pub start_time: Option<String>,
    pub init_container_statuses: Option<Vec<ContainerStatus>>,
    pub container_statuses: Option<Vec<ContainerStatus>>,
    pub qos_class: Option<String>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PodIp {
    pub ip: String,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PodCondition {
    /// `PodScheduled`, `Initialized`, `ContainersReady`, `Ready`, ...
    #[serde(rename = "type")]
    pub type_: String,
    pub status: String,
    pub last_probe_time: Option<String>,
    pub last_transition_time: Option<String>,
    pub reason: Option<String>,
    pub message: Option<String>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerStatus {
    pub name: String,
    pub state: Option<ContainerState>,
    pub last_state: Option<ContainerState>,
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub restart_count: i32,
    pub image: Option<String>,
    #[serde(rename = "imageID")]
    pub image_id: Option<String>,
    #[serde(rename = "containerID")]
    pub container_id: Option<String>,
    pub started: Option<bool>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerState {
    pub waiting: Option<ContainerStateWaiting>,
    pub running: Option<ContainerStateRunning>,
    pub terminated: Option<ContainerStateTerminated>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerStateWaiting {
    pub reason: Option<String>,
    pub message: Option<String>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerStateRunning {
    pub started_at: Option<String>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerStateTerminated {
    pub exit_code: i32,
    pub signal: Option<i32>,
    pub reason: Option<String>,
    pub message: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    #[serde(rename = "containerID")]
    pub container_id: Option<String>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PodTemplateSpec {
    pub metadata: Option<ObjectMeta>,
    pub spec: Option<PodSpec>,
}

// ---------------------------------------------------------------------------
// Node
// ---------------------------------------------------------------------------

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub api_version: Option<String>,
    pub kind: Option<String>,
    #[serde(default)]
    pub metadata: ObjectMeta,
    pub spec: Option<NodeSpec>,
    pub status: Option<NodeStatus>,
}
impl_resource!(Node, "", "v1", "Node", "nodes", false);

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeSpec {
    #[serde(rename = "podCIDR")]
    pub pod_cidr: Option<String>,
    #[serde(rename = "podCIDRs")]
    pub pod_cidrs: Option<Vec<String>>,
    #[serde(rename = "providerID")]
    pub provider_id: Option<String>,
    pub unschedulable: Option<bool>,
    pub taints: Option<Vec<Taint>>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Taint {
    pub key: String,
    pub value: Option<String>,
    pub effect: String,
    pub time_added: Option<String>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeStatus {
    pub capacity: Option<BTreeMap<String, String>>,
    pub allocatable: Option<BTreeMap<String, String>>,
    pub phase: Option<String>,
    pub conditions: Option<Vec<NodeCondition>>,
    pub addresses: Option<Vec<NodeAddress>>,
    pub daemon_endpoints: Option<Value>,
    pub node_info: Option<NodeSystemInfo>,
    pub images: Option<Vec<Value>>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeCondition {
    /// `Ready`, `MemoryPressure`, `DiskPressure`, `PIDPressure`, `NetworkUnavailable`
    #[serde(rename = "type")]
    pub type_: String,
    pub status: String,
    pub last_heartbeat_time: Option<String>,
    pub last_transition_time: Option<String>,
    pub reason: Option<String>,
    pub message: Option<String>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeAddress {
    /// `InternalIP`, `ExternalIP`, `Hostname`
    #[serde(rename = "type")]
    pub type_: String,
    pub address: String,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeSystemInfo {
    #[serde(rename = "machineID")]
    pub machine_id: Option<String>,
    #[serde(rename = "systemUUID")]
    pub system_uuid: Option<String>,
    #[serde(rename = "bootID")]
    pub boot_id: Option<String>,
    pub kernel_version: Option<String>,
    pub os_image: Option<String>,
    pub container_runtime_version: Option<String>,
    pub kubelet_version: Option<String>,
    pub kube_proxy_version: Option<String>,
    pub operating_system: Option<String>,
    pub architecture: Option<String>,
}

// ---------------------------------------------------------------------------
// Service / Endpoints
// ---------------------------------------------------------------------------

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Service {
    pub api_version: Option<String>,
    pub kind: Option<String>,
    #[serde(default)]
    pub metadata: ObjectMeta,
    pub spec: Option<ServiceSpec>,
    pub status: Option<ServiceStatus>,
}
impl_resource!(Service, "", "v1", "Service", "services", true);

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceSpec {
    /// `ClusterIP`, `NodePort`, `LoadBalancer`, `ExternalName`
    #[serde(rename = "type")]
    pub type_: Option<String>,
    pub selector: Option<BTreeMap<String, String>>,
    pub ports: Option<Vec<ServicePort>>,
    #[serde(rename = "clusterIP")]
    pub cluster_ip: Option<String>,
    #[serde(rename = "clusterIPs")]
    pub cluster_ips: Option<Vec<String>>,
    #[serde(rename = "externalIPs")]
    pub external_ips: Option<Vec<String>>,
    pub external_name: Option<String>,
    pub session_affinity: Option<String>,
    pub external_traffic_policy: Option<String>,
    pub internal_traffic_policy: Option<String>,
    pub ip_families: Option<Vec<String>>,
    pub ip_family_policy: Option<String>,
    #[serde(rename = "loadBalancerIP")]
    pub load_balancer_ip: Option<String>,
    pub publish_not_ready_addresses: Option<bool>,
    pub session_affinity_config: Option<SessionAffinityConfig>,
    pub health_check_node_port: Option<i32>,
    pub load_balancer_source_ranges: Option<Vec<String>>,
    pub allocate_load_balancer_node_ports: Option<bool>,
    pub load_balancer_class: Option<String>,
    pub traffic_distribution: Option<String>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionAffinityConfig {
    #[serde(rename = "clientIP")]
    pub client_ip: Option<ClientIPConfig>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientIPConfig {
    pub timeout_seconds: Option<i32>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServicePort {
    pub name: Option<String>,
    pub protocol: Option<String>,
    pub app_protocol: Option<String>,
    pub port: i32,
    pub target_port: Option<IntOrString>,
    pub node_port: Option<i32>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    pub load_balancer: Option<Value>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoints {
    pub api_version: Option<String>,
    pub kind: Option<String>,
    #[serde(default)]
    pub metadata: ObjectMeta,
    pub subsets: Option<Vec<EndpointSubset>>,
}
impl_resource!(Endpoints, "", "v1", "Endpoints", "endpoints", true);

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointSubset {
    pub addresses: Option<Vec<EndpointAddress>>,
    pub not_ready_addresses: Option<Vec<EndpointAddress>>,
    pub ports: Option<Vec<EndpointPort>>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointAddress {
    pub ip: String,
    pub hostname: Option<String>,
    pub node_name: Option<String>,
    pub target_ref: Option<ObjectReference>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointPort {
    pub name: Option<String>,
    pub port: i32,
    pub protocol: Option<String>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectReference {
    pub kind: Option<String>,
    pub namespace: Option<String>,
    pub name: Option<String>,
    pub uid: Option<String>,
    pub api_version: Option<String>,
    pub resource_version: Option<String>,
    pub field_path: Option<String>,
}

// ---------------------------------------------------------------------------
// ConfigMap / Secret / ServiceAccount
// ---------------------------------------------------------------------------

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigMap {
    pub api_version: Option<String>,
    pub kind: Option<String>,
    #[serde(default)]
    pub metadata: ObjectMeta,
    pub data: Option<BTreeMap<String, String>>,
    pub binary_data: Option<BTreeMap<String, String>>,
    pub immutable: Option<bool>,
}
impl_resource!(ConfigMap, "", "v1", "ConfigMap", "configmaps", true);

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Secret {
    pub api_version: Option<String>,
    pub kind: Option<String>,
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(rename = "type")]
    pub type_: Option<String>,
    /// base64-encoded values
    pub data: Option<BTreeMap<String, String>>,
    /// write-only convenience field, merged into `data` by the API server
    pub string_data: Option<BTreeMap<String, String>>,
    pub immutable: Option<bool>,
}
impl_resource!(Secret, "", "v1", "Secret", "secrets", true);

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceAccount {
    pub api_version: Option<String>,
    pub kind: Option<String>,
    #[serde(default)]
    pub metadata: ObjectMeta,
    pub secrets: Option<Vec<ObjectReference>>,
    pub automount_service_account_token: Option<bool>,
}
impl_resource!(ServiceAccount, "", "v1", "ServiceAccount", "serviceaccounts", true);

// ---------------------------------------------------------------------------
// Event
// ---------------------------------------------------------------------------

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub api_version: Option<String>,
    pub kind: Option<String>,
    #[serde(default)]
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub involved_object: ObjectReference,
    pub reason: Option<String>,
    pub message: Option<String>,
    pub source: Option<EventSource>,
    pub first_timestamp: Option<String>,
    pub last_timestamp: Option<String>,
    pub count: Option<i32>,
    /// `Normal` or `Warning`
    #[serde(rename = "type")]
    pub type_: Option<String>,
    pub event_time: Option<String>,
    pub reporting_component: Option<String>,
    pub reporting_instance: Option<String>,
}
impl_resource!(Event, "", "v1", "Event", "events", true);

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventSource {
    pub component: Option<String>,
    pub host: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Resource;

    #[test]
    fn unknown_fields_survive() {
        let raw = serde_json::json!({"containers": [{"name": "a", "image": "x", "resizePolicy": [{"resourceName": "cpu"}]}],
            "dnsConfig": {"nameservers": ["1.1.1.1"]}, "readinessGates": [{"conditionType": "x"}]});
        let spec: PodSpec = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(serde_json::to_value(&spec).unwrap(), raw);
    }

    #[test]
    fn pod_roundtrip_preserves_shape() {
        let raw = serde_json::json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "nginx", "labels": {"app": "web"}},
            "spec": {"containers": [{"name": "nginx", "image": "nginx:1.27",
                "ports": [{"containerPort": 80}]}],
                "volumes": [{"name": "data", "emptyDir": {}}]}
        });
        let pod: Pod = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(pod.metadata.name(), "nginx");
        assert_eq!(pod.spec.as_ref().unwrap().containers[0].ports.as_ref().unwrap()[0].container_port, 80);
        let back = serde_json::to_value(&pod).unwrap();
        assert_eq!(back, raw);
        assert_eq!(Pod::api_version(), "v1");
        assert_eq!(Pod::PLURAL, "pods");
    }
}

// ---------------------------------------------------------------------------
// PersistentVolume / PersistentVolumeClaim
// ---------------------------------------------------------------------------
//
// `spec` and `status` are kept as JSON: controllers read the fields they need
// (helpers in `k8s-component-helpers`) and write objects back without losing any.

/// `PersistentVolume`: a piece of storage in the cluster.
#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistentVolume {
    pub api_version: Option<String>,
    pub kind: Option<String>,
    #[serde(default)]
    pub metadata: ObjectMeta,
    pub spec: Option<Value>,
    pub status: Option<Value>,
}
impl_resource!(PersistentVolume, "", "v1", "PersistentVolume", "persistentvolumes", false);

/// `PersistentVolumeClaim`: a user's request for storage.
#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistentVolumeClaim {
    pub api_version: Option<String>,
    pub kind: Option<String>,
    #[serde(default)]
    pub metadata: ObjectMeta,
    pub spec: Option<Value>,
    pub status: Option<Value>,
}
impl_resource!(PersistentVolumeClaim, "", "v1", "PersistentVolumeClaim", "persistentvolumeclaims", true);
