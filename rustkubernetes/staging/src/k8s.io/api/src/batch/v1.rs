//! Port of `k8s.io/api/batch/v1` (Job, CronJob).

use k8s_apimachinery::meta::{LabelSelector, ObjectMeta};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_with::skip_serializing_none;

use crate::core::v1::{ObjectReference, PodTemplateSpec};
use crate::impl_resource;

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub api_version: Option<String>,
    pub kind: Option<String>,
    #[serde(default)]
    pub metadata: ObjectMeta,
    pub spec: Option<JobSpec>,
    pub status: Option<JobStatus>,
}
impl_resource!(Job, "batch", "v1", "Job", "jobs", true);

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSpec {
    pub parallelism: Option<i32>,
    pub completions: Option<i32>,
    pub active_deadline_seconds: Option<i64>,
    pub backoff_limit: Option<i32>,
    pub selector: Option<LabelSelector>,
    pub manual_selector: Option<bool>,
    #[serde(default)]
    pub template: PodTemplateSpec,
    pub ttl_seconds_after_finished: Option<i32>,
    /// `NonIndexed` or `Indexed`
    pub completion_mode: Option<String>,
    pub suspend: Option<bool>,
    pub pod_replacement_policy: Option<String>,
    pub pod_failure_policy: Option<Value>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobStatus {
    pub conditions: Option<Vec<JobCondition>>,
    pub start_time: Option<String>,
    pub completion_time: Option<String>,
    pub active: Option<i32>,
    pub succeeded: Option<i32>,
    pub failed: Option<i32>,
    pub terminating: Option<i32>,
    pub ready: Option<i32>,
    pub completed_indexes: Option<String>,
    pub failed_indexes: Option<String>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobCondition {
    /// `Complete`, `Failed`, `Suspended`, `FailureTarget`, `SuccessCriteriaMet`
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
pub struct CronJob {
    pub api_version: Option<String>,
    pub kind: Option<String>,
    #[serde(default)]
    pub metadata: ObjectMeta,
    pub spec: Option<CronJobSpec>,
    pub status: Option<CronJobStatus>,
}
impl_resource!(CronJob, "batch", "v1", "CronJob", "cronjobs", true);

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronJobSpec {
    pub schedule: String,
    pub time_zone: Option<String>,
    pub starting_deadline_seconds: Option<i64>,
    /// `Allow`, `Forbid`, `Replace`
    pub concurrency_policy: Option<String>,
    pub suspend: Option<bool>,
    #[serde(default)]
    pub job_template: JobTemplateSpec,
    pub successful_jobs_history_limit: Option<i32>,
    pub failed_jobs_history_limit: Option<i32>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobTemplateSpec {
    pub metadata: Option<ObjectMeta>,
    pub spec: Option<JobSpec>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronJobStatus {
    pub active: Option<Vec<ObjectReference>>,
    pub last_schedule_time: Option<String>,
    pub last_successful_time: Option<String>,
}
