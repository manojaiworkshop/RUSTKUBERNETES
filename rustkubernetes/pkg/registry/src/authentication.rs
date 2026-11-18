//! Port of `pkg/registry/authentication`: `selfsubjectreviews` (`kubectl auth
//! whoami`) and `tokenreviews` (webhook token authentication).

use async_trait::async_trait;
use k8s_apimachinery::errors::ApiError;
use k8s_apiserver::authentication::{current_user, AuthRequest};
use k8s_apiserver::{Strategy, StrategyContext};
use serde_json::{json, Value};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Review {
    SelfSubject,
    Token,
}

pub struct AuthnReviewStrategy(pub Review);

#[async_trait]
impl Strategy for AuthnReviewStrategy {
    fn is_virtual(&self) -> bool {
        true
    }

    async fn create_virtual(&self, ctx: &StrategyContext<'_>, _ns: &str, mut obj: Value) -> Result<Value, ApiError> {
        match self.0 {
            Review::SelfSubject => {
                let u = current_user().unwrap_or_default();
                obj["status"] = json!({"userInfo": u.to_json()});
            }
            Review::Token => {
                let token = obj["spec"]["token"].as_str().unwrap_or("").to_string();
                if token.is_empty() {
                    return Err(ApiError::invalid_simple("spec.token", "token must not be empty"));
                }
                let Some(chain) = ctx.authenticator else {
                    obj["status"] = json!({"authenticated": false, "error": "authentication is disabled"});
                    return Ok(obj);
                };
                let mut h = axum::http::HeaderMap::new();
                h.insert("authorization", format!("Bearer {token}").parse().map_err(|_| ApiError::bad_request("invalid token"))?);
                // Only token authenticators may vouch for a TokenReview (no anonymous fallback).
                obj["status"] = match chain.authenticate(&AuthRequest { headers: &h, peer_certs: None }).await {
                    Ok(u) if u.name != k8s_apiserver::authentication::USER_ANONYMOUS => {
                        json!({"authenticated": true, "user": u.to_json(), "audiences": obj["spec"]["audiences"].clone()})
                    }
                    Ok(_) => json!({"authenticated": false}),
                    Err(e) => json!({"authenticated": false, "error": e}),
                };
            }
        }
        Ok(obj)
    }
}
