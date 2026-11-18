//! `Priority` admission plugin (`plugin/pkg/admission/priority`).
//!
//! * Pods: resolves `spec.priorityClassName` to `spec.priority` (and
//!   `preemptionPolicy`); without a class the `globalDefault` class (or 0) is used.
//!   Unknown classes are rejected.
//! * PriorityClasses: only one may be `globalDefault`; values above
//!   1,000,000,000 are reserved for `system-` classes.

use async_trait::async_trait;
use k8s_apimachinery::errors::ApiError;
use k8s_apiserver::admission::{AdmissionPlugin, Attributes, Operation};
use k8s_apiserver::StrategyContext;
use serde_json::{json, Value};

pub const PLUGIN_NAME: &str = "Priority";
const HIGHEST_USER: i64 = 1_000_000_000;

pub struct Priority;

async fn classes(ctx: &StrategyContext<'_>) -> Result<Vec<Value>, ApiError> {
    let (items, _) =
        ctx.storage.list("/registry/priorityclasses.scheduling.k8s.io/").await.map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(items)
}

#[async_trait]
impl AdmissionPlugin for Priority {
    fn name(&self) -> &str {
        PLUGIN_NAME
    }

    async fn admit(&self, attrs: &Attributes<'_>, obj: &mut Value, ctx: &StrategyContext<'_>) -> Result<(), ApiError> {
        let r = attrs.resource;
        if attrs.operation != Operation::Create || !r.group.is_empty() || r.plural != "pods" {
            return Ok(());
        }
        let all = classes(ctx).await?;
        let name = obj.pointer("/spec/priorityClassName").and_then(Value::as_str).unwrap_or("").to_string();
        let class = if name.is_empty() {
            all.iter().find(|c| c["globalDefault"] == true)
        } else {
            Some(all.iter().find(|c| c.pointer("/metadata/name").and_then(Value::as_str) == Some(name.as_str())).ok_or_else(|| {
                ApiError::forbidden(format!("pods \"{}\" is forbidden: no PriorityClass with name {name} was found", attrs.name))
            })?)
        };
        let (value, policy) = match class {
            Some(c) => (c["value"].as_i64().unwrap_or(0), c["preemptionPolicy"].as_str().unwrap_or("PreemptLowerPriority").to_string()),
            None => (0, "PreemptLowerPriority".to_string()),
        };
        if let Some(p) = obj.pointer("/spec/priority").and_then(Value::as_i64) {
            if p != value {
                return Err(ApiError::forbidden(format!(
                    "pods \"{}\" is forbidden: the integer value of priority ({p}) must not be provided in pod spec; priority admission controller computed {value} from the given PriorityClass name",
                    attrs.name
                )));
            }
        }
        obj["spec"]["priority"] = json!(value);
        obj["spec"]["preemptionPolicy"] = json!(policy);
        if let Some(c) = class {
            obj["spec"]["priorityClassName"] = c["metadata"]["name"].clone();
        }
        Ok(())
    }

    async fn validate(&self, attrs: &Attributes<'_>, obj: &Value, ctx: &StrategyContext<'_>) -> Result<(), ApiError> {
        let r = attrs.resource;
        if r.group != "scheduling.k8s.io" || r.plural != "priorityclasses" || attrs.operation == Operation::Delete {
            return Ok(());
        }
        let name = attrs.name;
        let value = obj["value"].as_i64().unwrap_or(0);
        if value > HIGHEST_USER && !name.starts_with("system-") {
            return Err(ApiError::forbidden(format!(
                "priorityclasses.scheduling.k8s.io \"{name}\" is forbidden: maximum allowed value of a user defined priority is {HIGHEST_USER}"
            )));
        }
        if obj["globalDefault"] == true {
            let other = classes(ctx).await?.into_iter().find(|c| c["globalDefault"] == true && c["metadata"]["name"] != name);
            if let Some(o) = other {
                return Err(ApiError::forbidden(format!(
                    "priorityclasses.scheduling.k8s.io \"{name}\" is forbidden: PriorityClass {} is already marked as default. Only one default can exist",
                    o["metadata"]["name"].as_str().unwrap_or("")
                )));
            }
        }
        Ok(())
    }
}
