//! Port of `pkg/registry/authorization`: access reviews, computed per request
//! and never stored (`kubectl auth can-i`, kubelet / webhook authorizers).
//!
//! | Resource | Asks about |
//! |----------|------------|
//! | `selfsubjectaccessreviews` | the requesting user |
//! | `subjectaccessreviews` | any user in `spec.user` / `spec.groups` |
//! | `localsubjectaccessreviews` | any user, within the URL's namespace |
//! | `selfsubjectrulesreviews` | the requester's rules in `spec.namespace` (`can-i --list`) |

use async_trait::async_trait;
use k8s_apimachinery::errors::ApiError;
use k8s_apiserver::authentication::{current_user, UserInfo};
use k8s_apiserver::authorization::{Attributes, Decision};
use k8s_apiserver::{Strategy, StrategyContext};
use serde_json::{json, Value};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Review {
    SelfSubjectAccess,
    SubjectAccess,
    LocalSubjectAccess,
    SelfSubjectRules,
}

pub struct ReviewStrategy(pub Review);

fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or("").to_string()
}

/// `spec.resourceAttributes` / `spec.nonResourceAttributes` → authorizer attributes.
fn attributes(user: UserInfo, spec: &Value) -> Result<Attributes, ApiError> {
    if let Some(r) = spec.get("resourceAttributes").filter(|v| v.is_object()) {
        return Ok(Attributes {
            user,
            verb: s(r, "verb"),
            resource_request: true,
            namespace: s(r, "namespace"),
            api_group: s(r, "group"),
            api_version: s(r, "version"),
            resource: s(r, "resource"),
            subresource: s(r, "subresource"),
            name: s(r, "name"),
            ..Default::default()
        });
    }
    if let Some(n) = spec.get("nonResourceAttributes").filter(|v| v.is_object()) {
        return Ok(Attributes { user, verb: s(n, "verb"), path: s(n, "path"), ..Default::default() });
    }
    Err(ApiError::invalid_simple("spec.resourceAttributes", "exactly one of nonResourceAttributes or resourceAttributes must be specified"))
}

fn user_from_spec(spec: &Value) -> UserInfo {
    UserInfo {
        name: s(spec, "user"),
        uid: s(spec, "uid"),
        groups: spec["groups"].as_array().into_iter().flatten().filter_map(|g| g.as_str().map(String::from)).collect(),
        extra: spec["extra"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect()))
            .collect(),
    }
}

#[async_trait]
impl Strategy for ReviewStrategy {
    fn is_virtual(&self) -> bool {
        true
    }

    async fn create_virtual(&self, ctx: &StrategyContext<'_>, ns: &str, mut obj: Value) -> Result<Value, ApiError> {
        let spec = obj.get("spec").cloned().unwrap_or(json!({}));
        let me = current_user().unwrap_or_default();
        let status = match self.0 {
            Review::SelfSubjectRules => {
                let ns = s(&spec, "namespace");
                match ctx.authorizer.rules_for(&me, &ns).await {
                    Some(r) => {
                        json!({"resourceRules": r.resource_rules, "nonResourceRules": r.non_resource_rules, "incomplete": r.incomplete})
                    }
                    None => json!({"resourceRules": [], "nonResourceRules": [], "incomplete": true,
                                   "evaluationError": "the authorizer cannot list rules"}),
                }
            }
            _ => {
                let user = match self.0 {
                    Review::SelfSubjectAccess => me,
                    _ => {
                        if s(&spec, "user").is_empty() && spec["groups"].as_array().is_none_or(|g| g.is_empty()) {
                            return Err(ApiError::invalid_simple("spec.user", "at least one of user or group must be specified"));
                        }
                        user_from_spec(&spec)
                    }
                };
                let mut a = attributes(user, &spec)?;
                if self.0 == Review::LocalSubjectAccess {
                    if !a.namespace.is_empty() && a.namespace != ns {
                        return Err(ApiError::invalid_simple("spec.resourceAttributes.namespace", "must match metadata.namespace"));
                    }
                    a.namespace = ns.to_string();
                }
                match ctx.authorizer.authorize(&a).await {
                    Decision::Allow(r) => json!({"allowed": true, "reason": r}),
                    Decision::Deny(r) => json!({"allowed": false, "denied": true, "reason": r}),
                    Decision::NoOpinion(r) => json!({"allowed": false, "reason": r}),
                }
            }
        };
        obj["status"] = status;
        Ok(obj)
    }
}
