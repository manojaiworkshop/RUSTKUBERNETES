//! Port of `k8s.io/apimachinery/pkg/labels`: label selector parsing and matching.
//!
//! Supported syntax (same as `kubectl -l`):
//! `a=b`, `a==b`, `a!=b`, `a in (x,y)`, `a notin (x,y)`, `a`, `!a`, comma-separated (AND).

use std::collections::BTreeMap;
use std::fmt;

use crate::meta::LabelSelector;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operator {
    Equals,
    NotEquals,
    In,
    NotIn,
    Exists,
    DoesNotExist,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Requirement {
    pub key: String,
    pub op: Operator,
    pub values: Vec<String>,
}

impl Requirement {
    pub fn matches(&self, labels: &BTreeMap<String, String>) -> bool {
        let v = labels.get(&self.key);
        match self.op {
            Operator::Equals | Operator::In => v.is_some_and(|v| self.values.contains(v)),
            Operator::NotEquals | Operator::NotIn => v.is_none_or(|v| !self.values.contains(v)),
            Operator::Exists => v.is_some(),
            Operator::DoesNotExist => v.is_none(),
        }
    }
}

impl fmt::Display for Requirement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.op {
            Operator::Equals => write!(f, "{}={}", self.key, self.values[0]),
            Operator::NotEquals => write!(f, "{}!={}", self.key, self.values[0]),
            Operator::In => write!(f, "{} in ({})", self.key, self.values.join(",")),
            Operator::NotIn => write!(f, "{} notin ({})", self.key, self.values.join(",")),
            Operator::Exists => write!(f, "{}", self.key),
            Operator::DoesNotExist => write!(f, "!{}", self.key),
        }
    }
}

/// A parsed label selector: all requirements must match (logical AND).
/// An empty selector matches everything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selector {
    pub requirements: Vec<Requirement>,
    /// A selector that matches nothing (from a nil `*LabelSelector` in some contexts).
    pub nothing: bool,
}

impl Selector {
    pub fn everything() -> Self {
        Self::default()
    }

    pub fn nothing() -> Self {
        Self { requirements: vec![], nothing: true }
    }

    pub fn is_empty(&self) -> bool {
        self.requirements.is_empty() && !self.nothing
    }

    pub fn matches(&self, labels: &BTreeMap<String, String>) -> bool {
        !self.nothing && self.requirements.iter().all(|r| r.matches(labels))
    }

    /// Builds a selector that matches exactly the given label set.
    pub fn from_map(m: &BTreeMap<String, String>) -> Self {
        Self {
            requirements: m.iter().map(|(k, v)| Requirement { key: k.clone(), op: Operator::Equals, values: vec![v.clone()] }).collect(),
            nothing: false,
        }
    }

    /// Converts a `metav1.LabelSelector` (`matchLabels` + `matchExpressions`).
    pub fn from_label_selector(ls: &LabelSelector) -> Result<Self, String> {
        let mut reqs = vec![];
        for (k, v) in ls.match_labels.iter().flatten() {
            reqs.push(Requirement { key: k.clone(), op: Operator::Equals, values: vec![v.clone()] });
        }
        for e in ls.match_expressions.iter().flatten() {
            let values = e.values.clone().unwrap_or_default();
            let op = match e.operator.as_str() {
                "In" => Operator::In,
                "NotIn" => Operator::NotIn,
                "Exists" => Operator::Exists,
                "DoesNotExist" => Operator::DoesNotExist,
                other => return Err(format!("{other:?} is not a valid label selector operator")),
            };
            if matches!(op, Operator::In | Operator::NotIn) && values.is_empty() {
                return Err("values: Required value: must be specified when `operator` is 'In' or 'NotIn'".to_string());
            }
            reqs.push(Requirement { key: e.key.clone(), op, values });
        }
        Ok(Self { requirements: reqs, nothing: false })
    }

    /// Parses the string form used in `?labelSelector=` and `kubectl -l`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(Self::everything());
        }
        let mut reqs = vec![];
        for term in split_terms(s) {
            let term = term.trim();
            if term.is_empty() {
                continue;
            }
            reqs.push(parse_requirement(term)?);
        }
        Ok(Self { requirements: reqs, nothing: false })
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self.requirements.iter().map(|r| r.to_string()).collect();
        write!(f, "{}", parts.join(","))
    }
}

/// Splits on commas that are not inside parentheses.
fn split_terms(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut depth = 0;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '(' => {
                depth += 1;
                cur.push(c)
            }
            ')' => {
                depth -= 1;
                cur.push(c)
            }
            ',' if depth == 0 => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

fn parse_set(s: &str) -> Result<Vec<String>, String> {
    let s = s.trim();
    let inner = s.strip_prefix('(').and_then(|r| r.strip_suffix(')')).ok_or_else(|| format!("found '{s}', expected: '(' ... ')'"))?;
    Ok(inner.split(',').map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).collect())
}

fn parse_requirement(term: &str) -> Result<Requirement, String> {
    if let Some(key) = term.strip_prefix('!') {
        return Ok(Requirement { key: key.trim().to_string(), op: Operator::DoesNotExist, values: vec![] });
    }
    if let Some((k, v)) = term.split_once("!=") {
        return Ok(Requirement { key: k.trim().into(), op: Operator::NotEquals, values: vec![v.trim().into()] });
    }
    if let Some((k, v)) = term.split_once("==") {
        return Ok(Requirement { key: k.trim().into(), op: Operator::Equals, values: vec![v.trim().into()] });
    }
    if let Some((k, v)) = term.split_once('=') {
        return Ok(Requirement { key: k.trim().into(), op: Operator::Equals, values: vec![v.trim().into()] });
    }
    let words: Vec<&str> = term.splitn(2, char::is_whitespace).collect();
    if words.len() == 2 {
        let rest = words[1].trim_start();
        if let Some(set) = rest.strip_prefix("notin") {
            return Ok(Requirement { key: words[0].into(), op: Operator::NotIn, values: parse_set(set)? });
        }
        if let Some(set) = rest.strip_prefix("in") {
            return Ok(Requirement { key: words[0].into(), op: Operator::In, values: parse_set(set)? });
        }
        return Err(format!("unable to parse requirement: {term:?}"));
    }
    Ok(Requirement { key: term.to_string(), op: Operator::Exists, values: vec![] })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn parse_and_match() {
        let s = Selector::parse("app=web,tier!=db,env in (prod, staging),!legacy,team").unwrap();
        assert_eq!(s.requirements.len(), 5);
        assert!(s.matches(&m(&[("app", "web"), ("env", "prod"), ("team", "a")])));
        assert!(!s.matches(&m(&[("app", "web"), ("env", "dev"), ("team", "a")])));
        assert!(!s.matches(&m(&[("app", "web"), ("env", "prod"), ("team", "a"), ("legacy", "1")])));
        assert!(Selector::parse("").unwrap().matches(&m(&[])));
        assert!(!Selector::nothing().matches(&m(&[])));
    }

    #[test]
    fn notin() {
        let s = Selector::parse("env notin (prod)").unwrap();
        assert!(s.matches(&m(&[])));
        assert!(!s.matches(&m(&[("env", "prod")])));
    }
}
