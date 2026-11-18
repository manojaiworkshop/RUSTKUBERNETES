//! Port of `pkg/registry/core/pod` (strategy.go, storage/storage.go).

use async_trait::async_trait;
use k8s_apimachinery::errors::{ApiError, Status};
use k8s_apimachinery::meta::now_rfc3339;
use k8s_apimachinery::unstructured::ObjectExt;
use k8s_apimachinery::validation::{ErrorList, FieldError};
use k8s_apiserver::storage::parse_rv;
use k8s_apiserver::table::{age_cell, Column};
use k8s_apiserver::{Strategy, StrategyContext};
use kube_apis::core::{defaults, validation};
use serde_json::{json, Value};

use crate::generic::{int_at, str_at};

/// `kubelet_https`: reach kubelets over TLS (`--kubelet-client-certificate` set).
#[derive(Default)]
pub struct PodStrategy {
    pub kubelet_https: bool,
}

#[async_trait]
impl Strategy for PodStrategy {
    fn prepare_for_create(&self, obj: &mut Value) {
        defaults::set_pod_defaults(obj);
    }

    fn prepare_for_update(&self, new: &mut Value, _old: &Value) {
        if let Some(spec) = new.get_mut("spec") {
            defaults::set_pod_spec_defaults(spec);
        }
    }

    fn validate(&self, obj: &Value) -> ErrorList {
        validation::validate_pod(obj)
    }

    fn validate_update(&self, new: &Value, old: &Value) -> ErrorList {
        validation::validate_pod_update(new, old)
    }

    fn validate_status_update(&self, new: &Value, old: &Value) -> ErrorList {
        validation::validate_pod_status_update(new, old)
    }

    /// `CheckGracefulDelete`: unscheduled or terminal pods are removed immediately;
    /// running pods get their grace period so the kubelet can stop them.
    fn graceful_delete_seconds(&self, obj: &Value, requested: Option<i64>) -> Option<i64> {
        if str_at(obj, "/spec/nodeName").is_empty() {
            return Some(0);
        }
        if matches!(str_at(obj, "/status/phase"), "Succeeded" | "Failed") {
            return Some(0);
        }
        let grace = requested.unwrap_or_else(|| obj.pointer("/spec/terminationGracePeriodSeconds").and_then(Value::as_i64).unwrap_or(30));
        Some(grace.max(0))
    }

    async fn create_subresource(&self, ctx: &StrategyContext<'_>, sub: &str, pod: &Value, body: Value) -> Option<Result<Value, ApiError>> {
        match sub {
            "binding" => Some(bind(ctx, pod, body).await),
            _ => None,
        }
    }

    /// `pods/log`, `pods/exec`, `pods/attach` → the kubelet of the pod's node
    /// (`pkg/registry/core/pod/strategy.go: LogLocation, ExecLocation`).
    async fn connect(&self, ctx: &StrategyContext<'_>, sub: &str, pod: &Value, query: &str) -> Option<Result<String, ApiError>> {
        if !matches!(sub, "log" | "exec" | "attach") {
            return None;
        }
        Some(kubelet_location(ctx, sub, pod, query, self.kubelet_https).await)
    }

    fn table_columns(&self) -> Vec<Column> {
        vec![
            Column::new("Ready", "string", "The aggregate readiness state of this pod for accepting traffic."),
            Column::new("Status", "string", "The aggregate status of the containers in this pod."),
            Column::new("Restarts", "string", "The number of times the containers in this pod have been restarted."),
            Column::new("Age", "string", ""),
            Column::new("IP", "string", "The IP address assigned to the pod").wide(),
            Column::new("Node", "string", "The name of the node the pod is assigned to").wide(),
            Column::new("Nominated Node", "string", "The name of the node the pod is nominated on").wide(),
            Column::new("Readiness Gates", "string", "").wide(),
        ]
    }

    fn table_cells(&self, pod: &Value) -> Vec<Value> {
        let statuses = pod.pointer("/status/containerStatuses").and_then(Value::as_array).cloned().unwrap_or_default();
        let total = pod.pointer("/spec/containers").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
        let ready = statuses.iter().filter(|s| s["ready"] == true).count();
        let restarts: i64 = statuses.iter().map(|s| s["restartCount"].as_i64().unwrap_or(0)).sum();
        let none = |s: &str| if s.is_empty() { "<none>".to_string() } else { s.to_string() };
        vec![
            json!(format!("{ready}/{total}")),
            json!(pod_status_reason(pod)),
            json!(restarts.to_string()),
            age_cell(pod),
            json!(none(str_at(pod, "/status/podIP"))),
            json!(none(str_at(pod, "/spec/nodeName"))),
            json!(none(str_at(pod, "/status/nominatedNodeName"))),
            json!("<none>"),
        ]
    }
}

/// The STATUS column of `kubectl get pods` (simplified `printPod`).
pub fn pod_status_reason(pod: &Value) -> String {
    let mut reason = pod
        .pointer("/status/reason")
        .and_then(Value::as_str)
        .unwrap_or_else(|| pod.pointer("/status/phase").and_then(Value::as_str).unwrap_or("Pending"))
        .to_string();
    // Init containers
    if let Some(inits) = pod.pointer("/status/initContainerStatuses").and_then(Value::as_array) {
        let total = inits.len();
        for (i, s) in inits.iter().enumerate() {
            if let Some(t) = s.pointer("/state/terminated") {
                if int_at(t, "/exitCode") == 0 {
                    continue;
                }
                reason = format!("Init:{}", t["reason"].as_str().unwrap_or("Error"));
                break;
            }
            if let Some(w) = s.pointer("/state/waiting/reason").and_then(Value::as_str) {
                if w != "PodInitializing" {
                    reason = format!("Init:{w}");
                    break;
                }
            }
            reason = format!("Init:{i}/{total}");
            break;
        }
    }
    if !reason.starts_with("Init:") {
        for s in pod.pointer("/status/containerStatuses").and_then(Value::as_array).into_iter().flatten().rev() {
            if let Some(w) = s.pointer("/state/waiting/reason").and_then(Value::as_str) {
                reason = w.to_string();
            } else if let Some(t) = s.pointer("/state/terminated") {
                reason = t["reason"].as_str().map(String::from).unwrap_or_else(|| format!("ExitCode:{}", int_at(t, "/exitCode")));
            }
        }
    }
    if pod.deletion_timestamp().is_some() {
        reason = "Terminating".into();
    }
    reason
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| v.replace('+', " "))
}

async fn kubelet_location(ctx: &StrategyContext<'_>, sub: &str, pod: &Value, query: &str, https: bool) -> Result<String, ApiError> {
    let name = pod.name();
    let containers: Vec<String> = ["containers", "initContainers", "ephemeralContainers"]
        .iter()
        .flat_map(|k| pod.pointer(&format!("/spec/{k}")).and_then(Value::as_array).cloned().unwrap_or_default())
        .filter_map(|c| c["name"].as_str().map(String::from))
        .collect();
    let app: Vec<String> = pod
        .pointer("/spec/containers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c["name"].as_str().map(String::from))
        .collect();
    let container = match query_param(query, "container").filter(|c| !c.is_empty()) {
        Some(c) if containers.contains(&c) => c,
        Some(c) => return Err(ApiError::bad_request(format!("container {c} is not valid for pod {name}"))),
        None => match pod.pointer("/metadata/annotations/kubectl.kubernetes.io~1default-container").and_then(Value::as_str) {
            Some(d) => d.to_string(),
            None if app.len() == 1 => app[0].clone(),
            None => {
                return Err(ApiError::bad_request(format!(
                    "a container name must be specified for pod {name}, choose one of: [{}]",
                    app.join(" ")
                )))
            }
        },
    };
    let node = str_at(pod, "/spec/nodeName");
    if node.is_empty() {
        return Err(ApiError::bad_request(format!("pod {name} does not have a host assigned")));
    }
    let n = ctx.storage.get(&format!("/registry/nodes/{node}")).await.map_err(|_| ApiError::not_found("", "nodes", node))?;
    let ip = n
        .pointer("/status/addresses")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().find(|x| x["type"] == "InternalIP").or_else(|| a.first()))
        .and_then(|x| x["address"].as_str())
        .ok_or_else(|| ApiError::bad_request(format!("node {node} has no address")))?;
    let port = n.pointer("/status/daemonEndpoints/kubeletEndpoint/Port").and_then(Value::as_i64).unwrap_or(10250);
    let path = if sub == "log" { "containerLogs" } else { sub };
    let scheme = if https { "https" } else { "http" };
    Ok(format!("{scheme}://{ip}:{port}/{path}/{}/{name}/{container}?{query}", pod.namespace()))
}

/// `POST pods/<name>/binding`: assigns the pod to a node (used by the scheduler).
async fn bind(ctx: &StrategyContext<'_>, pod: &Value, binding: Value) -> Result<Value, ApiError> {
    let node = binding.pointer("/target/name").and_then(Value::as_str).unwrap_or("");
    if node.is_empty() {
        return Err(ApiError::invalid("", "Binding", pod.name(), &[FieldError::required("target.name", "")]));
    }
    if pod.deletion_timestamp().is_some() {
        return Err(ApiError::conflict("", "pods", pod.name(), "pod is being deleted, cannot be assigned to a host"));
    }
    let current = str_at(pod, "/spec/nodeName");
    if !current.is_empty() {
        return Err(ApiError::conflict("", "pods", pod.name(), &format!("pod {} is already assigned to node {current:?}", pod.name())));
    }
    let info = ctx.registry.lookup("", "v1", "pods").ok_or_else(|| ApiError::internal("pods not registered"))?;
    let mut new = pod.clone();
    new["spec"]["nodeName"] = json!(node);
    let conds = new["status"]["conditions"].as_array().cloned().unwrap_or_default();
    let mut conds: Vec<Value> = conds.into_iter().filter(|c| c["type"] != "PodScheduled").collect();
    conds.push(json!({"type": "PodScheduled", "status": "True", "lastProbeTime": null, "lastTransitionTime": now_rfc3339()}));
    new["status"]["conditions"] = json!(conds);
    let expected = parse_rv(pod.resource_version()).ok().flatten();
    ctx.storage
        .update(&info.key(pod.namespace(), pod.name()), expected, new)
        .await
        .map_err(|_| ApiError::conflict_modified("", "pods", pod.name()))?;
    Ok(serde_json::to_value(Status { code: Some(201), ..Status::success() }).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_reason() {
        let pod = json!({"status": {"phase": "Pending", "containerStatuses": [
            {"name": "a", "state": {"waiting": {"reason": "ContainerCreating"}}}]}});
        assert_eq!(pod_status_reason(&pod), "ContainerCreating");
        let pod = json!({"metadata": {"deletionTimestamp": "x"}, "status": {"phase": "Running"}});
        assert_eq!(pod_status_reason(&pod), "Terminating");
        let pod = json!({"status": {"phase": "Running", "containerStatuses": [
            {"name": "a", "state": {"terminated": {"exitCode": 1, "reason": "Error"}}}]}});
        assert_eq!(pod_status_reason(&pod), "Error");
    }

    #[test]
    fn graceful() {
        let s = PodStrategy::default();
        assert_eq!(s.graceful_delete_seconds(&json!({"spec": {}}), None), Some(0));
        assert_eq!(s.graceful_delete_seconds(&json!({"spec": {"nodeName": "n", "terminationGracePeriodSeconds": 30}}), None), Some(30));
        assert_eq!(s.graceful_delete_seconds(&json!({"spec": {"nodeName": "n"}}), Some(0)), Some(0));
    }
}
