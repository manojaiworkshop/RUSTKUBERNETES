//! `rbac.authorization.k8s.io/v1`.

pub mod v1 {
    use k8s_apimachinery::meta::{LabelSelector, ObjectMeta};
    use serde::{Deserialize, Serialize};
    use serde_with::skip_serializing_none;

    use crate::impl_resource;

    pub const GROUP: &str = "rbac.authorization.k8s.io";

    /// One permission (`PolicyRule`).
    #[skip_serializing_none]
    #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct PolicyRule {
        #[serde(default)]
        pub verbs: Vec<String>,
        pub api_groups: Option<Vec<String>>,
        pub resources: Option<Vec<String>>,
        pub resource_names: Option<Vec<String>>,
        #[serde(rename = "nonResourceURLs")]
        pub non_resource_urls: Option<Vec<String>>,
    }

    impl PolicyRule {
        /// Rule builder for resource permissions.
        pub fn resources(groups: &[&str], resources: &[&str], verbs: &[&str]) -> Self {
            Self {
                verbs: verbs.iter().map(|s| s.to_string()).collect(),
                api_groups: Some(groups.iter().map(|s| s.to_string()).collect()),
                resources: Some(resources.iter().map(|s| s.to_string()).collect()),
                ..Default::default()
            }
        }

        /// Rule builder for non-resource URLs.
        pub fn urls(urls: &[&str], verbs: &[&str]) -> Self {
            Self {
                verbs: verbs.iter().map(|s| s.to_string()).collect(),
                non_resource_urls: Some(urls.iter().map(|s| s.to_string()).collect()),
                ..Default::default()
            }
        }

        pub fn names(mut self, names: &[&str]) -> Self {
            self.resource_names = Some(names.iter().map(|s| s.to_string()).collect());
            self
        }
    }

    #[skip_serializing_none]
    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Role {
        pub api_version: Option<String>,
        pub kind: Option<String>,
        #[serde(default)]
        pub metadata: ObjectMeta,
        pub rules: Option<Vec<PolicyRule>>,
    }
    impl_resource!(Role, "rbac.authorization.k8s.io", "v1", "Role", "roles", true);

    #[skip_serializing_none]
    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct AggregationRule {
        pub cluster_role_selectors: Option<Vec<LabelSelector>>,
    }

    #[skip_serializing_none]
    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ClusterRole {
        pub api_version: Option<String>,
        pub kind: Option<String>,
        #[serde(default)]
        pub metadata: ObjectMeta,
        pub rules: Option<Vec<PolicyRule>>,
        pub aggregation_rule: Option<AggregationRule>,
    }
    impl_resource!(ClusterRole, "rbac.authorization.k8s.io", "v1", "ClusterRole", "clusterroles", false);

    /// `User`, `Group` or `ServiceAccount`.
    #[skip_serializing_none]
    #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Subject {
        pub kind: String,
        pub api_group: Option<String>,
        pub name: String,
        pub namespace: Option<String>,
    }

    impl Subject {
        pub fn user(name: &str) -> Self {
            Self { kind: "User".into(), api_group: Some(GROUP.into()), name: name.into(), namespace: None }
        }

        pub fn group(name: &str) -> Self {
            Self { kind: "Group".into(), api_group: Some(GROUP.into()), name: name.into(), namespace: None }
        }

        pub fn service_account(ns: &str, name: &str) -> Self {
            Self { kind: "ServiceAccount".into(), api_group: Some(String::new()), name: name.into(), namespace: Some(ns.into()) }
        }
    }

    #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct RoleRef {
        #[serde(default)]
        pub api_group: String,
        pub kind: String,
        pub name: String,
    }

    #[skip_serializing_none]
    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct RoleBinding {
        pub api_version: Option<String>,
        pub kind: Option<String>,
        #[serde(default)]
        pub metadata: ObjectMeta,
        pub subjects: Option<Vec<Subject>>,
        pub role_ref: RoleRef,
    }
    impl_resource!(RoleBinding, "rbac.authorization.k8s.io", "v1", "RoleBinding", "rolebindings", true);

    #[skip_serializing_none]
    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ClusterRoleBinding {
        pub api_version: Option<String>,
        pub kind: Option<String>,
        #[serde(default)]
        pub metadata: ObjectMeta,
        pub subjects: Option<Vec<Subject>>,
        pub role_ref: RoleRef,
    }
    impl_resource!(ClusterRoleBinding, "rbac.authorization.k8s.io", "v1", "ClusterRoleBinding", "clusterrolebindings", false);
}
