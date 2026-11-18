//! Port of `k8s.io/api/apps/v1` (Deployment, ReplicaSet, DaemonSet, StatefulSet).

use k8s_apimachinery::meta::{LabelSelector, ObjectMeta};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_with::skip_serializing_none;

use crate::core::v1::PodTemplateSpec;
use crate::{impl_resource, IntOrString};

// ---------------------------------------------------------------------------
// Deployment
// ---------------------------------------------------------------------------

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Deployment {
    pub api_version: Option<String>,
    pub kind: Option<String>,
    #[serde(default)]
    pub metadata: ObjectMeta,
    pub spec: Option<DeploymentSpec>,
    pub status: Option<DeploymentStatus>,
}
impl_resource!(Deployment, "apps", "v1", "Deployment", "deployments", true);

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentSpec {
    pub replicas: Option<i32>,
    pub selector: Option<LabelSelector>,
    #[serde(default)]
    pub template: PodTemplateSpec,
    pub strategy: Option<DeploymentStrategy>,
    pub min_ready_seconds: Option<i32>,
    pub revision_history_limit: Option<i32>,
    pub paused: Option<bool>,
    pub progress_deadline_seconds: Option<i32>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentStrategy {
    /// `RollingUpdate` or `Recreate`
    #[serde(rename = "type")]
    pub type_: Option<String>,
    pub rolling_update: Option<RollingUpdateDeployment>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RollingUpdateDeployment {
    pub max_unavailable: Option<IntOrString>,
    pub max_surge: Option<IntOrString>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentStatus {
    pub observed_generation: Option<i64>,
    pub replicas: Option<i32>,
    pub updated_replicas: Option<i32>,
    pub ready_replicas: Option<i32>,
    pub available_replicas: Option<i32>,
    pub unavailable_replicas: Option<i32>,
    pub conditions: Option<Vec<DeploymentCondition>>,
    pub collision_count: Option<i32>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentCondition {
    /// `Available`, `Progressing`, `ReplicaFailure`
    #[serde(rename = "type")]
    pub type_: String,
    pub status: String,
    pub last_update_time: Option<String>,
    pub last_transition_time: Option<String>,
    pub reason: Option<String>,
    pub message: Option<String>,
}

// ---------------------------------------------------------------------------
// ReplicaSet
// ---------------------------------------------------------------------------

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicaSet {
    pub api_version: Option<String>,
    pub kind: Option<String>,
    #[serde(default)]
    pub metadata: ObjectMeta,
    pub spec: Option<ReplicaSetSpec>,
    pub status: Option<ReplicaSetStatus>,
}
impl_resource!(ReplicaSet, "apps", "v1", "ReplicaSet", "replicasets", true);

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicaSetSpec {
    pub replicas: Option<i32>,
    pub min_ready_seconds: Option<i32>,
    pub selector: Option<LabelSelector>,
    pub template: Option<PodTemplateSpec>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicaSetStatus {
    #[serde(default)]
    pub replicas: i32,
    pub fully_labeled_replicas: Option<i32>,
    pub ready_replicas: Option<i32>,
    pub available_replicas: Option<i32>,
    pub observed_generation: Option<i64>,
    pub conditions: Option<Vec<Value>>,
}

// ---------------------------------------------------------------------------
// DaemonSet / StatefulSet (stored and validated; controllers in later phases)
// ---------------------------------------------------------------------------

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonSet {
    pub api_version: Option<String>,
    pub kind: Option<String>,
    #[serde(default)]
    pub metadata: ObjectMeta,
    pub spec: Option<DaemonSetSpec>,
    pub status: Option<DaemonSetStatus>,
}
impl_resource!(DaemonSet, "apps", "v1", "DaemonSet", "daemonsets", true);

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonSetSpec {
    pub selector: Option<LabelSelector>,
    #[serde(default)]
    pub template: PodTemplateSpec,
    pub update_strategy: Option<Value>,
    pub min_ready_seconds: Option<i32>,
    pub revision_history_limit: Option<i32>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatefulSet {
    pub api_version: Option<String>,
    pub kind: Option<String>,
    #[serde(default)]
    pub metadata: ObjectMeta,
    pub spec: Option<StatefulSetSpec>,
    pub status: Option<StatefulSetStatus>,
}
impl_resource!(StatefulSet, "apps", "v1", "StatefulSet", "statefulsets", true);

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatefulSetSpec {
    pub replicas: Option<i32>,
    pub selector: Option<LabelSelector>,
    #[serde(default)]
    pub template: PodTemplateSpec,
    pub service_name: Option<String>,
    pub volume_claim_templates: Option<Vec<Value>>,
    pub pod_management_policy: Option<String>,
    pub update_strategy: Option<Value>,
    pub revision_history_limit: Option<i32>,
    pub min_ready_seconds: Option<i32>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonSetStatus {
    #[serde(default)]
    pub current_number_scheduled: i32,
    #[serde(default)]
    pub number_misscheduled: i32,
    #[serde(default)]
    pub desired_number_scheduled: i32,
    #[serde(default)]
    pub number_ready: i32,
    pub observed_generation: Option<i64>,
    pub updated_number_scheduled: Option<i32>,
    pub number_available: Option<i32>,
    pub number_unavailable: Option<i32>,
    pub collision_count: Option<i32>,
    pub conditions: Option<Vec<Value>>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatefulSetStatus {
    pub observed_generation: Option<i64>,
    #[serde(default)]
    pub replicas: i32,
    pub ready_replicas: Option<i32>,
    pub current_replicas: Option<i32>,
    pub updated_replicas: Option<i32>,
    pub available_replicas: Option<i32>,
    pub current_revision: Option<String>,
    pub update_revision: Option<String>,
    pub collision_count: Option<i32>,
    pub conditions: Option<Vec<Value>>,
}
