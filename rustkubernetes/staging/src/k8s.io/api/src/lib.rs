//! Rust port of `k8s.io/api`: typed external API objects.
//!
//! Layout mirrors the Go module: `core::v1`, `apps::v1`, `batch::v1`,
//! `coordination::v1`, `discovery::v1`.
//! Every top-level object implements [`Resource`], which carries the static
//! information (group, version, kind, plural, scope) that clients need to
//! build URLs.

use k8s_apimachinery::meta::ObjectMeta;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub mod apps;
pub mod batch;
pub mod certificates;
pub mod coordination;
pub mod core;
pub mod discovery;
pub mod protobuf;
pub mod rbac;
pub mod scheduling;
pub mod storage;

/// Static type information for a top-level API object.
pub trait Resource: Serialize + DeserializeOwned + Clone + Send + Sync + std::fmt::Debug + 'static {
    const GROUP: &'static str;
    const VERSION: &'static str;
    const KIND: &'static str;
    /// Lowercase plural resource name used in URLs (`pods`).
    const PLURAL: &'static str;
    const NAMESPACED: bool;

    fn metadata(&self) -> &ObjectMeta;
    fn metadata_mut(&mut self) -> &mut ObjectMeta;

    /// `v1` or `apps/v1`.
    fn api_version() -> String {
        if Self::GROUP.is_empty() {
            Self::VERSION.to_string()
        } else {
            format!("{}/{}", Self::GROUP, Self::VERSION)
        }
    }
}

/// `intstr.IntOrString`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IntOrString {
    Int(i64),
    String(String),
}

impl Default for IntOrString {
    fn default() -> Self {
        IntOrString::Int(0)
    }
}

impl IntOrString {
    /// Resolves a value that may be a percentage (`"25%"`) against `total`.
    /// `round_up` mirrors `intstr.GetScaledValueFromIntOrPercent`.
    pub fn scaled_value(&self, total: i64, round_up: bool) -> Result<i64, String> {
        match self {
            IntOrString::Int(i) => Ok(*i),
            IntOrString::String(s) => {
                let pct: f64 = s
                    .strip_suffix('%')
                    .ok_or_else(|| format!("invalid value for IntOrString: invalid type: string is not a percentage: {s}"))?
                    .parse()
                    .map_err(|_| format!("invalid value {s:?}"))?;
                let v = pct * total as f64 / 100.0;
                Ok(if round_up { v.ceil() as i64 } else { v.floor() as i64 })
            }
        }
    }
}

impl std::fmt::Display for IntOrString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IntOrString::Int(i) => write!(f, "{i}"),
            IntOrString::String(s) => f.write_str(s),
        }
    }
}

/// Implements [`Resource`] for a struct with `api_version`, `kind` and `metadata` fields.
#[macro_export]
macro_rules! impl_resource {
    ($t:ty, $group:expr, $version:expr, $kind:expr, $plural:expr, $namespaced:expr) => {
        impl $crate::Resource for $t {
            const GROUP: &'static str = $group;
            const VERSION: &'static str = $version;
            const KIND: &'static str = $kind;
            const PLURAL: &'static str = $plural;
            const NAMESPACED: bool = $namespaced;
            fn metadata(&self) -> &k8s_apimachinery::meta::ObjectMeta {
                &self.metadata
            }
            fn metadata_mut(&mut self) -> &mut k8s_apimachinery::meta::ObjectMeta {
                &mut self.metadata
            }
        }

        impl $t {
            /// Creates an empty object with `apiVersion`/`kind` set.
            pub fn new(metadata: k8s_apimachinery::meta::ObjectMeta) -> Self {
                Self {
                    api_version: Some(<$t as $crate::Resource>::api_version()),
                    kind: Some($kind.to_string()),
                    metadata,
                    ..Default::default()
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_or_string() {
        assert_eq!(IntOrString::String("25%".into()).scaled_value(10, true).unwrap(), 3);
        assert_eq!(IntOrString::String("25%".into()).scaled_value(10, false).unwrap(), 2);
        assert_eq!(IntOrString::Int(4).scaled_value(10, false).unwrap(), 4);
        let v: IntOrString = serde_json::from_str("8080").unwrap();
        assert_eq!(v, IntOrString::Int(8080));
        let v: IntOrString = serde_json::from_str("\"http\"").unwrap();
        assert_eq!(v, IntOrString::String("http".into()));
    }
}
