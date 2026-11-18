//! Port of `k8s.io/apiserver/pkg/endpoints/request` (`RequestInfoFactory`).
//!
//! Turns a URL path into a structured request:
//!
//! ```text
//! /api                                   → ApiVersions
//! /apis                                  → ApiGroupList
//! /apis/apps                             → ApiGroup("apps")
//! /api/v1 , /apis/apps/v1                → ResourceList
//! /api/v1/nodes/n1                       → Resource{resource: nodes, name: n1}
//! /api/v1/namespaces/default/pods/p/log  → Resource{ns: default, resource: pods, name: p, sub: log}
//! /api/v1/namespaces/default/status      → Resource{resource: namespaces, name: default, sub: status}
//! ```

use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceRequest {
    pub group: String,
    pub version: String,
    pub resource: String,
    pub namespace: String,
    pub name: Option<String>,
    pub subresource: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestTarget {
    ApiVersions,
    ApiGroupList,
    ApiGroup(String),
    ResourceList { group: String, version: String },
    Resource(ResourceRequest),
    NotFound,
}

/// Namespace subresources that must not be treated as resource names.
const NAMESPACE_SUBRESOURCES: &[&str] = &["status", "finalize"];

pub fn parse_path(path: &str) -> RequestTarget {
    let parts: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    match parts.as_slice() {
        ["api"] => RequestTarget::ApiVersions,
        ["apis"] => RequestTarget::ApiGroupList,
        ["apis", group] => RequestTarget::ApiGroup(group.to_string()),
        ["api", version] => RequestTarget::ResourceList { group: String::new(), version: version.to_string() },
        ["apis", group, version] => RequestTarget::ResourceList { group: group.to_string(), version: version.to_string() },
        ["api", version, rest @ ..] => parse_resource("", version, rest),
        ["apis", group, version, rest @ ..] => parse_resource(group, version, rest),
        _ => RequestTarget::NotFound,
    }
}

fn parse_resource(group: &str, version: &str, parts: &[&str]) -> RequestTarget {
    let mut parts = parts;
    let mut namespace = String::new();
    if parts.first() == Some(&"namespaces") && parts.len() > 1 {
        namespace = parts[1].to_string();
        if parts.len() > 2 && !NAMESPACE_SUBRESOURCES.contains(&parts[2]) {
            parts = &parts[2..];
        } else {
            // A request on the namespace object itself.
            namespace = String::new();
        }
    }
    let req = ResourceRequest {
        group: group.into(),
        version: version.into(),
        resource: parts[0].to_string(),
        namespace,
        name: parts.get(1).map(|s| s.to_string()),
        subresource: if parts.len() > 2 { Some(parts[2..].join("/")) } else { None },
    };
    RequestTarget::Resource(req)
}

/// Parsed query string.
#[derive(Clone, Debug, Default)]
pub struct Query(pub HashMap<String, String>, pub Option<String>);

impl Query {
    pub fn parse(q: Option<&str>) -> Self {
        Self(form_urlencoded::parse(q.unwrap_or("").as_bytes()).into_owned().collect(), q.map(String::from))
    }

    /// The raw query string (keeps repeated parameters).
    pub fn raw(&self) -> Option<&str> {
        self.1.as_deref()
    }

    pub fn get(&self, k: &str) -> Option<&str> {
        self.0.get(k).map(String::as_str).filter(|s| !s.is_empty())
    }

    pub fn flag(&self, k: &str) -> bool {
        matches!(self.get(k), Some("true") | Some("1"))
    }

    pub fn int(&self, k: &str) -> Option<i64> {
        self.get(k).and_then(|v| v.parse().ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(g: &str, r: &str, ns: &str, name: Option<&str>, sub: Option<&str>) -> RequestTarget {
        RequestTarget::Resource(ResourceRequest {
            group: g.into(),
            version: "v1".into(),
            resource: r.into(),
            namespace: ns.into(),
            name: name.map(Into::into),
            subresource: sub.map(Into::into),
        })
    }

    #[test]
    fn paths() {
        assert_eq!(parse_path("/api"), RequestTarget::ApiVersions);
        assert_eq!(parse_path("/apis/apps"), RequestTarget::ApiGroup("apps".into()));
        assert_eq!(parse_path("/api/v1/pods"), res("", "pods", "", None, None));
        assert_eq!(parse_path("/api/v1/namespaces"), res("", "namespaces", "", None, None));
        assert_eq!(parse_path("/api/v1/namespaces/default"), res("", "namespaces", "", Some("default"), None));
        assert_eq!(parse_path("/api/v1/namespaces/default/status"), res("", "namespaces", "", Some("default"), Some("status")));
        assert_eq!(parse_path("/api/v1/namespaces/ns/pods"), res("", "pods", "ns", None, None));
        assert_eq!(parse_path("/api/v1/namespaces/ns/pods/p/status"), res("", "pods", "ns", Some("p"), Some("status")));
        assert_eq!(parse_path("/apis/apps/v1/namespaces/ns/deployments/d"), res("apps", "deployments", "ns", Some("d"), None));
    }
}
