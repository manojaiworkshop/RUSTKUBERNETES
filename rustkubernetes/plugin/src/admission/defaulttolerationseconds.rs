//! `DefaultTolerationSeconds` admission plugin.
//!
//! Adds `node.kubernetes.io/not-ready` and `node.kubernetes.io/unreachable`
//! `NoExecute` tolerations with `tolerationSeconds: 300` to every pod that does
//! not already tolerate those taints, so pods survive short node outages before
//! the taint-eviction controller evicts them.

use async_trait::async_trait;
use k8s_apimachinery::errors::ApiError;
use k8s_apiserver::admission::{AdmissionPlugin, Attributes, Operation};
use k8s_apiserver::StrategyContext;
use serde_json::{json, Value};

pub const PLUGIN_NAME: &str = "DefaultTolerationSeconds";
pub const TAINT_NOT_READY: &str = "node.kubernetes.io/not-ready";
pub const TAINT_UNREACHABLE: &str = "node.kubernetes.io/unreachable";
pub const DEFAULT_SECONDS: i64 = 300;

pub struct DefaultTolerationSeconds;

fn tolerates_key(tols: &[Value], key: &str) -> bool {
    tols.iter().any(|t| {
        let k = t["key"].as_str().unwrap_or("");
        let effect = t["effect"].as_str().unwrap_or("");
        (k == key || (k.is_empty() && t["operator"] == "Exists")) && (effect.is_empty() || effect == "NoExecute")
    })
}

#[async_trait]
impl AdmissionPlugin for DefaultTolerationSeconds {
    fn name(&self) -> &str {
        PLUGIN_NAME
    }

    async fn admit(&self, attrs: &Attributes<'_>, obj: &mut Value, _ctx: &StrategyContext<'_>) -> Result<(), ApiError> {
        if attrs.operation != Operation::Create || !attrs.resource.group.is_empty() || attrs.resource.plural != "pods" {
            return Ok(());
        }
        let mut tols = obj.pointer("/spec/tolerations").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut changed = false;
        for key in [TAINT_NOT_READY, TAINT_UNREACHABLE] {
            if !tolerates_key(&tols, key) {
                tols.push(json!({"key": key, "operator": "Exists", "effect": "NoExecute", "tolerationSeconds": DEFAULT_SECONDS}));
                changed = true;
            }
        }
        if changed {
            obj["spec"]["tolerations"] = json!(tols);
        }
        Ok(())
    }
}
