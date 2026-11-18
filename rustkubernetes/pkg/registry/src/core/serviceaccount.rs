//! Port of `pkg/registry/core/serviceaccount`: ServiceAccounts and their
//! `token` subresource (`TokenRequest`, used by `kubectl create token` and by the
//! kubelet for projected `serviceAccountToken` volumes).

use std::sync::Arc;

use async_trait::async_trait;
use k8s_apimachinery::errors::ApiError;
use k8s_apimachinery::unstructured::ObjectExt;
use k8s_apiserver::table::{age_cell, Column};
use k8s_apiserver::{Strategy, StrategyContext};
use kube_serviceaccount::{KubernetesClaims, Ref, TokenGenerator};
use serde_json::{json, Value};

/// `--service-account-max-token-expiration` default is unlimited; tokens are at least 10 minutes.
pub const MIN_EXPIRATION_SECONDS: i64 = 600;
pub const DEFAULT_EXPIRATION_SECONDS: i64 = 3600;

pub struct ServiceAccountStrategy {
    /// `None` when the server has no `--service-account-signing-key-file`.
    pub tokens: Option<Arc<TokenGenerator>>,
    /// `--api-audiences` (default: the issuer).
    pub audiences: Vec<String>,
    pub max_expiration: Option<i64>,
}

fn rfc3339(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0).map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string()).unwrap_or_default()
}

#[async_trait]
impl Strategy for ServiceAccountStrategy {
    fn table_columns(&self) -> Vec<Column> {
        vec![Column::new("Secrets", "string", ""), Column::new("Age", "string", "")]
    }

    fn table_cells(&self, o: &Value) -> Vec<Value> {
        vec![json!(o.get("secrets").and_then(Value::as_array).map(Vec::len).unwrap_or(0).to_string()), age_cell(o)]
    }

    async fn create_subresource(
        &self,
        ctx: &StrategyContext<'_>,
        sub: &str,
        sa: &Value,
        mut body: Value,
    ) -> Option<Result<Value, ApiError>> {
        if sub != "token" {
            return None;
        }
        Some(self.token_request(ctx, sa, &mut body).await.map(|_| body))
    }
}

impl ServiceAccountStrategy {
    async fn token_request(&self, ctx: &StrategyContext<'_>, sa: &Value, req: &mut Value) -> Result<(), ApiError> {
        let gen = self.tokens.as_ref().ok_or_else(|| {
            ApiError::new(404, "NotFound", "the token subresource is not enabled (no --service-account-signing-key-file)")
        })?;
        let ns = sa.namespace().to_string();
        let spec = req.get("spec").cloned().unwrap_or(json!({}));
        let mut ttl = spec["expirationSeconds"].as_i64().unwrap_or(DEFAULT_EXPIRATION_SECONDS);
        if ttl < MIN_EXPIRATION_SECONDS {
            return Err(ApiError::invalid_simple("spec.expirationSeconds", "may not specify a duration less than 10 minutes"));
        }
        if let Some(max) = self.max_expiration {
            ttl = ttl.min(max);
        }
        let audiences: Vec<String> = match spec["audiences"].as_array().filter(|a| !a.is_empty()) {
            Some(a) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
            None => self.audiences.clone(),
        };
        let mut kc = KubernetesClaims {
            namespace: ns.clone(),
            serviceaccount: Ref { name: sa.name().to_string(), uid: sa["metadata"]["uid"].as_str().unwrap_or("").to_string() },
            ..Default::default()
        };
        // Bind the token to an object: it stops working when that object is deleted.
        if let Some(b) = spec.get("boundObjectRef").filter(|b| b.is_object()) {
            let kind = b["kind"].as_str().unwrap_or("");
            let name = b["name"].as_str().unwrap_or("");
            let plural = match kind {
                "Pod" => "pods",
                "Secret" => "secrets",
                _ => {
                    return Err(ApiError::invalid_simple(
                        "spec.boundObjectRef.kind",
                        &format!("unsupported kind {kind:?}: want Pod or Secret"),
                    ))
                }
            };
            let obj =
                ctx.storage.get(&format!("/registry/{plural}/{ns}/{name}")).await.map_err(|_| ApiError::not_found("", plural, name))?;
            let uid = obj["metadata"]["uid"].as_str().unwrap_or("").to_string();
            if let Some(want) = b["uid"].as_str().filter(|u| !u.is_empty()) {
                if want != uid {
                    return Err(ApiError::new(
                        409,
                        "Conflict",
                        format!("the UID in the bound object reference ({want}) does not match the UID in record ({uid})"),
                    ));
                }
            }
            let r = Some(Ref { name: name.to_string(), uid });
            if kind == "Pod" {
                kc.pod = r;
            } else {
                kc.secret = r;
            }
        }
        let (token, exp) = gen.generate(&audiences, ttl, kc).map_err(ApiError::internal)?;
        req["spec"]["expirationSeconds"] = json!(ttl);
        req["spec"]["audiences"] = json!(audiences);
        req["status"] = json!({"token": token, "expirationTimestamp": rfc3339(exp)});
        req["kind"] = json!("TokenRequest");
        req["apiVersion"] = json!("authentication.k8s.io/v1");
        Ok(())
    }
}
