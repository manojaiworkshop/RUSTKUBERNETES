//! Port of `k8s.io/apimachinery/pkg/fields`: field selectors.
//!
//! Supports `path=value`, `path==value`, `path!=value`, comma-separated (AND).
//! Paths are dotted JSON paths into the object, e.g. `metadata.name`,
//! `spec.nodeName`, `status.phase`.

use serde_json::Value;

use crate::unstructured::get_path_str;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldRequirement {
    pub path: String,
    pub value: String,
    pub negate: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FieldSelector {
    pub requirements: Vec<FieldRequirement>,
}

impl FieldSelector {
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut reqs = vec![];
        for term in s.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            let (path, value, negate) = if let Some((p, v)) = term.split_once("!=") {
                (p, v, true)
            } else if let Some((p, v)) = term.split_once("==") {
                (p, v, false)
            } else if let Some((p, v)) = term.split_once('=') {
                (p, v, false)
            } else {
                return Err(format!("invalid selector: '{term}'; can't understand '{term}'"));
            };
            reqs.push(FieldRequirement { path: path.trim().into(), value: value.trim().into(), negate });
        }
        Ok(Self { requirements: reqs })
    }

    pub fn is_empty(&self) -> bool {
        self.requirements.is_empty()
    }

    /// Returns the value required for `path` (an equality requirement), if any.
    pub fn required_value(&self, path: &str) -> Option<&str> {
        self.requirements.iter().find(|r| r.path == path && !r.negate).map(|r| r.value.as_str())
    }

    pub fn matches(&self, obj: &Value) -> bool {
        self.requirements.iter().all(|r| {
            let actual = get_path_str(obj, &r.path).unwrap_or_default();
            (actual == r.value) != r.negate
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn field_match() {
        let pod = json!({"metadata": {"name": "a"}, "spec": {"nodeName": "n1"}, "status": {"phase": "Running"}});
        assert!(FieldSelector::parse("metadata.name=a,spec.nodeName=n1").unwrap().matches(&pod));
        assert!(FieldSelector::parse("status.phase!=Failed").unwrap().matches(&pod));
        assert!(!FieldSelector::parse("spec.nodeName=").unwrap().matches(&pod));
        assert!(FieldSelector::parse("spec.schedulerName=").unwrap().matches(&pod));
    }
}
