//! Port of `k8s.io/apimachinery/pkg/apis/meta/v1` core types.

use std::collections::BTreeMap;

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;

/// `TypeMeta` describes an individual object in an API response or request
/// with strings representing the type of the object and its API schema version.
#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeMeta {
    pub api_version: Option<String>,
    pub kind: Option<String>,
}

/// `ObjectMeta` is metadata that all persisted resources must have.
#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectMeta {
    pub name: Option<String>,
    pub generate_name: Option<String>,
    pub namespace: Option<String>,
    pub uid: Option<String>,
    pub resource_version: Option<String>,
    pub generation: Option<i64>,
    pub creation_timestamp: Option<String>,
    pub deletion_timestamp: Option<String>,
    pub deletion_grace_period_seconds: Option<i64>,
    pub labels: Option<BTreeMap<String, String>>,
    pub annotations: Option<BTreeMap<String, String>>,
    pub owner_references: Option<Vec<OwnerReference>>,
    pub finalizers: Option<Vec<String>>,
}

impl ObjectMeta {
    pub fn named(name: impl Into<String>) -> Self {
        Self { name: Some(name.into()), ..Default::default() }
    }

    pub fn namespaced(namespace: impl Into<String>, name: impl Into<String>) -> Self {
        Self { name: Some(name.into()), namespace: Some(namespace.into()), ..Default::default() }
    }

    pub fn name(&self) -> &str {
        self.name.as_deref().unwrap_or("")
    }

    pub fn namespace(&self) -> &str {
        self.namespace.as_deref().unwrap_or("")
    }

    pub fn labels(&self) -> BTreeMap<String, String> {
        self.labels.clone().unwrap_or_default()
    }

    /// Returns the owner reference with `controller: true`, if any.
    pub fn controller_ref(&self) -> Option<&OwnerReference> {
        self.owner_references.as_ref()?.iter().find(|r| r.controller == Some(true))
    }
}

/// `OwnerReference` contains enough information to let you identify an owning object.
#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerReference {
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub uid: String,
    pub controller: Option<bool>,
    pub block_owner_deletion: Option<bool>,
}

/// `ListMeta` describes metadata that synthetic resources must have (lists).
#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListMeta {
    pub resource_version: Option<String>,
    #[serde(rename = "continue")]
    pub continue_: Option<String>,
    pub remaining_item_count: Option<i64>,
}

/// Generic list wrapper (`PodList`, `NodeList`, ...).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", bound(deserialize = "T: serde::de::DeserializeOwned"))]
pub struct List<T> {
    #[serde(flatten)]
    pub type_meta: TypeMeta,
    #[serde(default)]
    pub metadata: ListMeta,
    #[serde(default)]
    pub items: Vec<T>,
}

/// A label selector (`matchLabels` + `matchExpressions`).
#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelSelector {
    pub match_labels: Option<BTreeMap<String, String>>,
    pub match_expressions: Option<Vec<LabelSelectorRequirement>>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelSelectorRequirement {
    pub key: String,
    /// `In`, `NotIn`, `Exists`, `DoesNotExist`
    pub operator: String,
    pub values: Option<Vec<String>>,
}

/// Standard condition type (`metav1.Condition`).
#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    #[serde(rename = "type")]
    pub type_: String,
    pub status: String,
    pub observed_generation: Option<i64>,
    pub last_transition_time: Option<String>,
    pub reason: Option<String>,
    pub message: Option<String>,
}

/// Returns the current time in Kubernetes RFC3339 (second precision, `Z`) format.
pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Parses an RFC3339 timestamp.
pub fn parse_time(s: &str) -> Option<chrono::DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|t| t.with_timezone(&Utc))
}

/// Human readable age like `kubectl get` prints it (`5s`, `3m`, `2h`, `4d`).
pub fn human_age(timestamp: &str) -> String {
    let Some(t) = parse_time(timestamp) else { return "<unknown>".into() };
    let secs = (Utc::now() - t).num_seconds().max(0);
    match secs {
        s if s < 120 => format!("{s}s"),
        s if s < 60 * 60 => format!("{}m", s / 60),
        s if s < 48 * 3600 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

/// Generates a fresh UID.
pub fn new_uid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Random 5-character suffix used for `generateName` (same alphabet as Go's
/// `utilrand.String`, which avoids vowels and confusable characters).
pub fn random_suffix(n: usize) -> String {
    const ALPHANUMS: &[u8] = b"bcdfghjklmnpqrstvwxz2456789";
    let bytes = uuid::Uuid::new_v4().into_bytes();
    (0..n).map(|i| ALPHANUMS[bytes[i % 16] as usize % ALPHANUMS.len()] as char).collect()
}
