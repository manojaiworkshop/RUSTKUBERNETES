//! Port of `k8s.io/component-helpers`.
//!
//! | Module | Go |
//! |--------|----|
//! | [`storage::volume`] | `storage/volume/{helpers,pv_helpers}.go` |
//! | [`scheduling::nodeaffinity`] | `scheduling/corev1/nodeaffinity` |

pub mod storage;

pub mod scheduling {
    /// `NodeSelector` matching (`nodeaffinity.NewNodeSelector(...).Match`).
    pub mod nodeaffinity {
        use std::collections::BTreeMap;

        use serde_json::Value;

        /// One requirement (`In`, `NotIn`, `Exists`, `DoesNotExist`, `Gt`, `Lt`).
        pub fn requirement_matches(req: &Value, values_of: &BTreeMap<String, String>) -> bool {
            let key = req["key"].as_str().unwrap_or("");
            let values: Vec<&str> = req["values"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let v = values_of.get(key);
            let int = |s: &str| s.parse::<i64>().ok();
            match req["operator"].as_str().unwrap_or("") {
                "In" => v.is_some_and(|v| values.contains(&v.as_str())),
                "NotIn" => v.is_none_or(|v| !values.contains(&v.as_str())),
                "Exists" => v.is_some(),
                "DoesNotExist" => v.is_none(),
                "Gt" => v.and_then(|v| int(v)).zip(values.first().and_then(|x| int(x))).is_some_and(|(a, b)| a > b),
                "Lt" => v.and_then(|v| int(v)).zip(values.first().and_then(|x| int(x))).is_some_and(|(a, b)| a < b),
                _ => false,
            }
        }

        /// A `NodeSelector` (`{"nodeSelectorTerms": [...]}`): terms are ORed, the
        /// expressions and fields of a term ANDed; an empty term matches nothing.
        pub fn node_selector_matches(sel: &Value, labels: &BTreeMap<String, String>, node_name: &str) -> bool {
            let fields: BTreeMap<String, String> = [("metadata.name".to_string(), node_name.to_string())].into();
            sel["nodeSelectorTerms"].as_array().into_iter().flatten().any(|t| {
                let exprs = t["matchExpressions"].as_array().cloned().unwrap_or_default();
                let mf = t["matchFields"].as_array().cloned().unwrap_or_default();
                (!exprs.is_empty() || !mf.is_empty())
                    && exprs.iter().all(|r| requirement_matches(r, labels))
                    && mf.iter().all(|r| requirement_matches(r, &fields))
            })
        }
    }
}
