//! Port of `pkg/registry/rbac`: Roles, ClusterRoles and their bindings, with
//! privilege-escalation prevention (`rbac/validation.ConfirmNoEscalation`).
//!
//! A user may only create or update a role whose permissions they already hold
//! (or with the `escalate` verb on roles/clusterroles), and may only bind a role
//! whose permissions they hold (or with the `bind` verb on that role). The check
//! asks the cluster's authorizer for every verb × group × resource × name of the
//! new rules, so it works with any authorizer chain.

use async_trait::async_trait;
use k8s_api::rbac::v1::{PolicyRule, RoleRef, GROUP};
use k8s_apimachinery::errors::ApiError;
use k8s_apimachinery::unstructured::ObjectExt;
use k8s_apimachinery::validation::{ErrorList, FieldError};
use k8s_apiserver::authentication::{current_user, UserInfo};
use k8s_apiserver::authorization::{Attributes, Authorizer, Decision};
use k8s_apiserver::table::{age_cell, Column};
use k8s_apiserver::{Strategy, StrategyContext};
use serde_json::{json, Value};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Role,
    ClusterRole,
    RoleBinding,
    ClusterRoleBinding,
}

pub struct RbacStrategy(pub Kind);

fn rules_of(obj: &Value) -> Vec<PolicyRule> {
    obj.get("rules").cloned().and_then(|r| serde_json::from_value(r).ok()).unwrap_or_default()
}

fn validate_rules(obj: &Value, errs: &mut ErrorList) {
    for (i, r) in rules_of(obj).iter().enumerate() {
        let f = format!("rules[{i}]");
        if r.verbs.is_empty() {
            errs.push(FieldError::required(format!("{f}.verbs"), "verbs must contain at least one value"));
        }
        let has_urls = r.non_resource_urls.as_ref().is_some_and(|u| !u.is_empty());
        let has_res = r.resources.as_ref().is_some_and(|u| !u.is_empty());
        if has_urls && (has_res || r.api_groups.as_ref().is_some_and(|g| !g.is_empty())) {
            errs.push(FieldError::invalid(
                format!("{f}.nonResourceURLs"),
                "",
                "rules cannot apply to both regular resources and non-resource URLs",
            ));
        }
        if !has_urls && !has_res {
            errs.push(FieldError::required(format!("{f}.resources"), "resource rules must supply at least one resource"));
        }
        if has_res && r.api_groups.as_ref().is_none_or(|g| g.is_empty()) {
            errs.push(FieldError::required(format!("{f}.apiGroups"), "resource rules must supply at least one api group"));
        }
    }
}

fn validate_binding(obj: &Value, namespaced: bool, errs: &mut ErrorList) {
    let rr = &obj["roleRef"];
    let kind = rr["kind"].as_str().unwrap_or("");
    let ok_kinds: &[&str] = if namespaced { &["Role", "ClusterRole"] } else { &["ClusterRole"] };
    if !ok_kinds.contains(&kind) {
        errs.push(FieldError::not_supported("roleRef.kind", kind, ok_kinds));
    }
    if rr["apiGroup"].as_str().unwrap_or("") != GROUP {
        errs.push(FieldError::not_supported("roleRef.apiGroup", rr["apiGroup"].as_str().unwrap_or(""), &[GROUP]));
    }
    if rr["name"].as_str().unwrap_or("").is_empty() {
        errs.push(FieldError::required("roleRef.name", ""));
    }
    for (i, s) in obj["subjects"].as_array().into_iter().flatten().enumerate() {
        let k = s["kind"].as_str().unwrap_or("");
        if !["User", "Group", "ServiceAccount"].contains(&k) {
            errs.push(FieldError::not_supported(format!("subjects[{i}].kind"), k, &["ServiceAccount", "User", "Group"]));
        }
        if s["name"].as_str().unwrap_or("").is_empty() {
            errs.push(FieldError::required(format!("subjects[{i}].name"), ""));
        }
        if k == "ServiceAccount" && !namespaced && s["namespace"].as_str().unwrap_or("").is_empty() {
            errs.push(FieldError::required(format!("subjects[{i}].namespace"), ""));
        }
    }
}

async fn allowed(z: &dyn Authorizer, a: &Attributes) -> bool {
    matches!(z.authorize(a).await, Decision::Allow(_))
}

/// Every atomic permission of `rules` the user lacks (`ConfirmNoEscalation`).
pub async fn missing_permissions(z: &dyn Authorizer, user: &UserInfo, ns: &str, rules: &[PolicyRule]) -> Vec<String> {
    let mut missing = vec![];
    for r in rules {
        for verb in &r.verbs {
            if let Some(urls) = r.non_resource_urls.as_ref().filter(|u| !u.is_empty()) {
                for u in urls {
                    let a = Attributes { user: user.clone(), verb: verb.clone(), path: u.clone(), ..Default::default() };
                    if !allowed(z, &a).await {
                        missing.push(format!("{{NonResourceURLs:[\"{u}\"], Verbs:[\"{verb}\"]}}"));
                    }
                }
                continue;
            }
            let names: Vec<String> = r.resource_names.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| vec![String::new()]);
            for g in r.api_groups.iter().flatten() {
                for res in r.resources.iter().flatten() {
                    let (resource, sub) = res.split_once('/').unwrap_or((res, ""));
                    for n in &names {
                        let a = Attributes {
                            user: user.clone(),
                            verb: verb.clone(),
                            resource_request: true,
                            namespace: ns.into(),
                            api_group: g.clone(),
                            resource: resource.into(),
                            subresource: sub.into(),
                            name: n.clone(),
                            ..Default::default()
                        };
                        if !allowed(z, &a).await {
                            let rn = if n.is_empty() { String::new() } else { format!(", ResourceNames:[\"{n}\"]") };
                            missing.push(format!("{{APIGroups:[\"{g}\"], Resources:[\"{res}\"]{rn}, Verbs:[\"{verb}\"]}}"));
                        }
                    }
                }
            }
        }
    }
    missing
}

fn escalation_error(kind: &str, name: &str, user: &UserInfo, missing: &[String]) -> ApiError {
    let groups: Vec<String> = user.groups.iter().map(|g| format!("\"{g}\"")).collect();
    ApiError::forbidden(format!(
        "{kind}.rbac.authorization.k8s.io \"{name}\" is forbidden: user \"{}\" (groups=[{}]) is attempting to grant RBAC permissions not currently held:\n{}",
        user.name,
        groups.join(" "),
        missing.join("\n")
    ))
    .with_details(GROUP, kind, name)
}

fn verb_on(user: &UserInfo, verb: &str, resource: &str, ns: &str, name: &str) -> Attributes {
    Attributes {
        user: user.clone(),
        verb: verb.into(),
        resource_request: true,
        namespace: ns.into(),
        api_group: GROUP.into(),
        resource: resource.into(),
        name: name.into(),
        ..Default::default()
    }
}

async fn referenced_rules(ctx: &StrategyContext<'_>, ns: &str, r: &RoleRef) -> Result<Vec<PolicyRule>, ApiError> {
    let key = match r.kind.as_str() {
        "Role" => format!("/registry/roles/{ns}/{}", r.name),
        _ => format!("/registry/clusterroles/{}", r.name),
    };
    match ctx.storage.get(&key).await {
        Ok(obj) => Ok(rules_of(&obj)),
        Err(_) => Err(ApiError::not_found(GROUP, &format!("{}s", r.kind.to_lowercase()), &r.name)),
    }
}

impl RbacStrategy {
    fn resource(&self) -> &'static str {
        match self.0 {
            Kind::Role => "roles",
            Kind::ClusterRole => "clusterroles",
            Kind::RoleBinding => "rolebindings",
            Kind::ClusterRoleBinding => "clusterrolebindings",
        }
    }

    async fn check(&self, ctx: &StrategyContext<'_>, obj: &Value) -> Result<(), ApiError> {
        // Requests from the server itself (bootstrap policy) are trusted.
        let Some(user) = current_user() else { return Ok(()) };
        let ns = obj.namespace().to_string();
        let name = obj.name().to_string();
        match self.0 {
            Kind::Role | Kind::ClusterRole => {
                if allowed(ctx.authorizer, &verb_on(&user, "escalate", self.resource(), &ns, &name)).await {
                    return Ok(());
                }
                // Aggregated ClusterRoles get their rules from other roles: check those instead.
                let missing = missing_permissions(ctx.authorizer, &user, &ns, &rules_of(obj)).await;
                if !missing.is_empty() {
                    return Err(escalation_error(self.resource(), &name, &user, &missing));
                }
            }
            Kind::RoleBinding | Kind::ClusterRoleBinding => {
                let rr: RoleRef = serde_json::from_value(obj["roleRef"].clone()).map_err(|e| ApiError::bad_request(e.to_string()))?;
                let role_res = if rr.kind == "Role" { "roles" } else { "clusterroles" };
                if allowed(ctx.authorizer, &verb_on(&user, "bind", role_res, &ns, &rr.name)).await {
                    return Ok(());
                }
                let rules = referenced_rules(ctx, &ns, &rr).await?;
                let missing = missing_permissions(ctx.authorizer, &user, &ns, &rules).await;
                if !missing.is_empty() {
                    return Err(escalation_error(self.resource(), &name, &user, &missing));
                }
            }
        }
        Ok(())
    }
}

/// `path.ValidatePathSegmentName`: RBAC names may contain `:` (`system:node`).
pub fn validate_path_segment_name(name: &str) -> Vec<String> {
    if name == "." || name == ".." {
        return vec![format!("may not be '{name}'")];
    }
    ["/", "%"].iter().filter(|c| name.contains(**c)).map(|c| format!("may not contain '{c}'")).collect()
}

#[async_trait]
impl Strategy for RbacStrategy {
    fn validate_name(&self, name: &str) -> Vec<String> {
        validate_path_segment_name(name)
    }

    fn validate(&self, obj: &Value) -> ErrorList {
        let mut e = ErrorList::new();
        match self.0 {
            Kind::Role | Kind::ClusterRole => validate_rules(obj, &mut e),
            Kind::RoleBinding => validate_binding(obj, true, &mut e),
            Kind::ClusterRoleBinding => validate_binding(obj, false, &mut e),
        }
        e
    }

    fn validate_update(&self, new: &Value, old: &Value) -> ErrorList {
        let mut e = self.validate(new);
        if matches!(self.0, Kind::RoleBinding | Kind::ClusterRoleBinding) && new["roleRef"] != old["roleRef"] {
            e.push(FieldError::invalid("roleRef", "", "cannot change roleRef"));
        }
        e
    }

    async fn before_create(&self, ctx: &StrategyContext<'_>, obj: &mut Value) -> Result<(), ApiError> {
        self.check(ctx, obj).await
    }

    async fn before_update(&self, ctx: &StrategyContext<'_>, new: &mut Value, _old: &Value) -> Result<(), ApiError> {
        self.check(ctx, new).await
    }

    fn table_columns(&self) -> Vec<Column> {
        match self.0 {
            Kind::Role | Kind::ClusterRole => vec![Column::new("Created At", "date", "")],
            _ => vec![
                Column::new("Role", "string", ""),
                Column::new("Age", "string", ""),
                Column::new("Users", "string", "").wide(),
                Column::new("Groups", "string", "").wide(),
                Column::new("ServiceAccounts", "string", "").wide(),
            ],
        }
    }

    fn table_cells(&self, obj: &Value) -> Vec<Value> {
        match self.0 {
            Kind::Role | Kind::ClusterRole => vec![obj["metadata"]["creationTimestamp"].clone()],
            _ => {
                let subjects = obj["subjects"].as_array().cloned().unwrap_or_default();
                let of = |k: &str| {
                    subjects
                        .iter()
                        .filter(|s| s["kind"] == k)
                        .map(|s| match k {
                            "ServiceAccount" => {
                                format!("{}/{}", s["namespace"].as_str().unwrap_or(obj.namespace()), s["name"].as_str().unwrap_or(""))
                            }
                            _ => s["name"].as_str().unwrap_or("").to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                vec![
                    json!(format!("{}/{}", obj["roleRef"]["kind"].as_str().unwrap_or(""), obj["roleRef"]["name"].as_str().unwrap_or(""))),
                    age_cell(obj),
                    json!(of("User")),
                    json!(of("Group")),
                    json!(of("ServiceAccount")),
                ]
            }
        }
    }
}
