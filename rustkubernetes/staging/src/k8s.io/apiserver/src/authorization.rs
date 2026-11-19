//! Port of `k8s.io/apiserver/pkg/authorization`: may this user do this?
//!
//! Each request becomes [`Attributes`] (`RequestInfoFactory` verbs: get, list,
//! watch, create, update, patch, delete, deletecollection; non-resource paths
//! use the HTTP method lowercased). Authorizers are asked in order
//! (`union.New`); the first `Allow` or `Deny` decides, `NoOpinion` falls through,
//! and a request nobody allowed is forbidden.

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::Method;
use k8s_apimachinery::errors::ApiError;
use serde_json::{json, Value};

use crate::authentication::{UserInfo, GROUP_MASTERS};
use crate::request::{parse_path, Query, RequestTarget};

/// `authorizer.Attributes`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attributes {
    pub user: UserInfo,
    pub verb: String,
    pub resource_request: bool,
    pub namespace: String,
    pub api_group: String,
    pub api_version: String,
    pub resource: String,
    pub subresource: String,
    pub name: String,
    /// Request path for non-resource requests (`/healthz`, `/api`).
    pub path: String,
    /// `fieldSelector` of list/watch requests (`GetFieldSelector`).
    pub field_selector: String,
    /// `labelSelector` of list/watch requests (`GetLabelSelector`).
    pub label_selector: String,
}

impl Attributes {
    /// Exact-match requirements of the field selector (`field=value` / `field==value`).
    pub fn field_requirements(&self) -> Vec<(&str, &str)> {
        selector_terms(&self.field_selector)
    }

    /// `pods` or `pods/log`.
    pub fn resource_with_sub(&self) -> String {
        if self.subresource.is_empty() {
            self.resource.clone()
        } else {
            format!("{}/{}", self.resource, self.subresource)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow(String),
    Deny(String),
    NoOpinion(String),
}

/// Rules a user has in a namespace (`SelfSubjectRulesReview`).
#[derive(Clone, Debug, Default)]
pub struct RuleSet {
    pub resource_rules: Vec<Value>,
    pub non_resource_rules: Vec<Value>,
    pub incomplete: bool,
}

#[async_trait]
pub trait Authorizer: Send + Sync {
    async fn authorize(&self, a: &Attributes) -> Decision;

    /// Rules the user has in `namespace`, if this authorizer can enumerate them.
    async fn rules_for(&self, _user: &UserInfo, _namespace: &str) -> Option<RuleSet> {
        None
    }
}

/// `authorizerfactory.NewAlwaysAllowAuthorizer`.
pub struct AlwaysAllow;

#[async_trait]
impl Authorizer for AlwaysAllow {
    async fn authorize(&self, _a: &Attributes) -> Decision {
        Decision::Allow(String::new())
    }

    async fn rules_for(&self, _user: &UserInfo, _namespace: &str) -> Option<RuleSet> {
        Some(RuleSet {
            resource_rules: vec![json!({"verbs": ["*"], "apiGroups": ["*"], "resources": ["*"]})],
            non_resource_rules: vec![json!({"verbs": ["*"], "nonResourceURLs": ["*"]})],
            incomplete: false,
        })
    }
}

/// `system:masters` may do anything (`authorizerfactory.NewPrivilegedGroups`).
pub struct PrivilegedGroups;

#[async_trait]
impl Authorizer for PrivilegedGroups {
    async fn authorize(&self, a: &Attributes) -> Decision {
        if a.user.in_group(GROUP_MASTERS) {
            Decision::Allow(String::new())
        } else {
            Decision::NoOpinion(String::new())
        }
    }

    async fn rules_for(&self, user: &UserInfo, ns: &str) -> Option<RuleSet> {
        if user.in_group(GROUP_MASTERS) {
            AlwaysAllow.rules_for(user, ns).await
        } else {
            None
        }
    }
}

pub struct AuthorizerChain(pub Vec<Arc<dyn Authorizer>>);

#[async_trait]
impl Authorizer for AuthorizerChain {
    async fn authorize(&self, a: &Attributes) -> Decision {
        let mut reasons = vec![];
        for z in &self.0 {
            match z.authorize(a).await {
                Decision::NoOpinion(r) => {
                    if !r.is_empty() {
                        reasons.push(r);
                    }
                }
                d => return d,
            }
        }
        Decision::NoOpinion(reasons.join(", "))
    }

    async fn rules_for(&self, user: &UserInfo, ns: &str) -> Option<RuleSet> {
        let mut out = RuleSet::default();
        let mut any = false;
        for z in &self.0 {
            if let Some(r) = z.rules_for(user, ns).await {
                any = true;
                out.resource_rules.extend(r.resource_rules);
                out.non_resource_rules.extend(r.non_resource_rules);
                out.incomplete |= r.incomplete;
            }
        }
        any.then_some(out)
    }
}

/// Builds the attributes of an HTTP request (`WithRequestInfo` + `GetAuthorizerAttributes`).
pub fn request_attributes(user: &UserInfo, method: &Method, path: &str, q: &Query, websocket: bool) -> Attributes {
    let mut a = Attributes { user: user.clone(), path: path.to_string(), ..Default::default() };
    match parse_path(path) {
        RequestTarget::Resource(r) => {
            a.resource_request = true;
            a.api_group = r.group;
            a.api_version = r.version;
            a.resource = r.resource;
            a.namespace = r.namespace;
            a.subresource = r.subresource.unwrap_or_default();
            a.name = r.name.clone().unwrap_or_default();
            let named = r.name.is_some();
            a.verb = match *method {
                // exec/attach/portforward over WebSocket are GETs that create a session.
                Method::GET if websocket => "create",
                Method::GET if q.flag("watch") => "watch",
                Method::GET | Method::HEAD if named => "get",
                Method::GET | Method::HEAD => "list",
                Method::POST => "create",
                Method::PUT => "update",
                Method::PATCH => "patch",
                Method::DELETE if named => "delete",
                Method::DELETE => "deletecollection",
                _ => "",
            }
            .to_string();
            if a.verb == "list" || a.verb == "watch" {
                a.field_selector = q.get("fieldSelector").unwrap_or_default().to_string();
                a.label_selector = q.get("labelSelector").unwrap_or_default().to_string();
                // A list/watch of one object (`?fieldSelector=metadata.name=x`) names it
                // (`RequestInfoFactory`: `FieldSelector.RequiresExactMatch("metadata.name")`).
                if !named {
                    if let Some((_, n)) = a.field_requirements().into_iter().find(|(f, _)| *f == "metadata.name") {
                        if !n.is_empty() && !n.contains('/') && n != "." && n != ".." {
                            a.name = n.to_string();
                        }
                    }
                }
            }
        }
        _ => {
            a.verb = method.as_str().to_lowercase();
        }
    }
    a
}

/// `field=value` and `field==value` terms of a selector (`!=` terms are skipped).
pub fn selector_terms(sel: &str) -> Vec<(&str, &str)> {
    sel.split(',')
        .filter_map(|t| {
            let t = t.trim();
            if t.contains("!=") {
                return None;
            }
            let (k, v) = t.split_once("==").or_else(|| t.split_once('='))?;
            Some((k.trim(), v.trim()))
        })
        .collect()
}

/// The 403 a client sees (`responsewriters.Forbidden`).
pub fn forbidden(a: &Attributes, reason: &str) -> ApiError {
    let user = &a.user.name;
    let suffix = if reason.is_empty() { String::new() } else { format!(": {reason}") };
    if !a.resource_request {
        return ApiError::forbidden(format!("forbidden: User \"{user}\" cannot {} path \"{}\"{suffix}", a.verb, a.path));
    }
    let res = a.resource_with_sub();
    let what = if a.name.is_empty() { res.clone() } else { format!("{res} \"{}\"", a.name) };
    let scope = if a.namespace.is_empty() { "at the cluster scope".to_string() } else { format!("in the namespace \"{}\"", a.namespace) };
    ApiError::forbidden(format!(
        "{what} is forbidden: User \"{user}\" cannot {} resource \"{res}\" in API group \"{}\" {scope}{suffix}",
        a.verb, a.api_group
    ))
    .with_details(&a.api_group, &a.resource, &a.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(method: Method, path: &str, query: Option<&str>) -> Attributes {
        request_attributes(&UserInfo::new("u", &[]), &method, path, &Query::parse(query), false)
    }

    #[test]
    fn verbs() {
        let a = attrs(Method::GET, "/api/v1/namespaces/default/pods", None);
        assert_eq!((a.verb.as_str(), a.resource.as_str(), a.namespace.as_str()), ("list", "pods", "default"));
        assert_eq!(attrs(Method::GET, "/api/v1/namespaces/default/pods/p", None).verb, "get");
        let w = attrs(Method::GET, "/api/v1/namespaces/default/pods", Some("watch=true&fieldSelector=metadata.name%3Dp"));
        assert_eq!((w.verb.as_str(), w.name.as_str()), ("watch", "p"));
        let l = attrs(Method::GET, "/api/v1/namespaces/default/pods/p/log", None);
        assert_eq!((l.verb.as_str(), l.subresource.as_str()), ("get", "log"));
        assert_eq!(attrs(Method::DELETE, "/apis/apps/v1/namespaces/x/deployments", None).verb, "deletecollection");
        let n = attrs(Method::GET, "/healthz", None);
        assert!(!n.resource_request && n.verb == "get");
        let e = request_attributes(&UserInfo::new("u", &[]), &Method::GET, "/api/v1/namespaces/d/pods/p/exec", &Query::parse(None), true);
        assert_eq!((e.verb.as_str(), e.subresource.as_str()), ("create", "exec"));
    }

    #[test]
    fn forbidden_message() {
        let a = attrs(Method::GET, "/api/v1/namespaces/default/pods", None);
        let e = forbidden(&a, "");
        assert_eq!(e.message, "pods is forbidden: User \"u\" cannot list resource \"pods\" in API group \"\" in the namespace \"default\"");
        let a = attrs(Method::GET, "/api", None);
        assert_eq!(forbidden(&a, "").message, "forbidden: User \"u\" cannot get path \"/api\"");
    }
}
