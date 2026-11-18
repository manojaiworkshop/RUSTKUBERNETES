//! `endpoints/filters/impersonation.go`: `Impersonate-User`, `Impersonate-Group`,
//! `Impersonate-Uid` and `Impersonate-Extra-<key>` headers (`kubectl --as`,
//! `--as-group`). The requester needs the `impersonate` verb on `users`,
//! `groups`, `serviceaccounts`, `uids` or `userextras/<key>` for each value.

use axum::http::HeaderMap;
use k8s_apimachinery::errors::ApiError;

use crate::authentication::{UserInfo, GROUP_AUTHENTICATED, GROUP_UNAUTHENTICATED, USER_ANONYMOUS};
use crate::authorization::{forbidden, Attributes, Authorizer, Decision};

fn values<'a>(h: &'a HeaderMap, name: &str) -> Vec<&'a str> {
    h.get_all(name).iter().filter_map(|v| v.to_str().ok()).collect()
}

async fn check(z: &dyn Authorizer, user: &UserInfo, group: &str, resource: &str, ns: &str, name: &str) -> Result<(), ApiError> {
    let (resource, sub) = resource.split_once('/').unwrap_or((resource, ""));
    let a = Attributes {
        user: user.clone(),
        verb: "impersonate".into(),
        resource_request: true,
        api_group: group.into(),
        resource: resource.into(),
        subresource: sub.into(),
        namespace: ns.into(),
        name: name.into(),
        ..Default::default()
    };
    match z.authorize(&a).await {
        Decision::Allow(_) => Ok(()),
        Decision::Deny(r) | Decision::NoOpinion(r) => Err(forbidden(&a, &r)),
    }
}

/// Returns the user the request acts as.
pub async fn impersonate(z: &dyn Authorizer, user: UserInfo, h: &HeaderMap) -> Result<UserInfo, ApiError> {
    let users = values(h, "impersonate-user");
    let groups = values(h, "impersonate-group");
    let uid = values(h, "impersonate-uid");
    let extras: Vec<(String, &str)> = h
        .iter()
        .filter_map(|(k, v)| k.as_str().strip_prefix("impersonate-extra-").map(|key| (key.to_string(), v.to_str().unwrap_or(""))))
        .collect();
    if users.is_empty() {
        if !groups.is_empty() || !uid.is_empty() || !extras.is_empty() {
            return Err(ApiError::bad_request("requested impersonation of groups, uid or extra without impersonating a user"));
        }
        return Ok(user);
    }
    let name = users[0];
    let mut out = UserInfo { name: name.to_string(), ..Default::default() };
    if let Some(sa) = name.strip_prefix("system:serviceaccount:") {
        let (ns, sa) = sa.split_once(':').ok_or_else(|| ApiError::bad_request(format!("invalid service account name {name:?}")))?;
        check(z, &user, "", "serviceaccounts", ns, sa).await?;
        if groups.is_empty() {
            out.groups = vec!["system:serviceaccounts".into(), format!("system:serviceaccounts:{ns}")];
        }
    } else {
        check(z, &user, "", "users", "", name).await?;
    }
    for g in &groups {
        check(z, &user, "", "groups", "", g).await?;
        out.groups.push(g.to_string());
    }
    if let Some(u) = uid.first() {
        check(z, &user, "authentication.k8s.io", "uids", "", u).await?;
        out.uid = u.to_string();
    }
    for (k, v) in extras {
        check(z, &user, "authentication.k8s.io", &format!("userextras/{k}"), "", v).await?;
        out.extra.entry(k).or_default().push(v.to_string());
    }
    let implicit = if name == USER_ANONYMOUS { GROUP_UNAUTHENTICATED } else { GROUP_AUTHENTICATED };
    if !out.in_group(implicit) {
        out.groups.push(implicit.into());
    }
    tracing::debug!(by = %user.name, as_user = %out.name, groups = ?out.groups, "impersonating");
    Ok(out)
}
