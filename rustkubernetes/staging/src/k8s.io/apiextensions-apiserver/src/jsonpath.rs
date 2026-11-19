//! The subset of `k8s.io/client-go/util/jsonpath` that CRD
//! `additionalPrinterColumns` use: `.a.b`, `.a[0]`, `.a[*].b`, `.a['k.with.dots']`
//! and filters like `.status.conditions[?(@.type=="Ready")].status`.

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
enum Step {
    Field(String),
    Index(i64),
    All,
    /// `[?(@.path == literal)]` (also `!=`)
    Filter {
        path: Vec<Step>,
        negate: bool,
        literal: Value,
    },
}

#[derive(Clone, Debug)]
pub struct JsonPath {
    steps: Vec<Step>,
}

impl JsonPath {
    /// Parses a path such as `.spec.replicas` (with or without surrounding `{}`).
    pub fn parse(src: &str) -> Result<Self, String> {
        let src = src.trim();
        let src = src.strip_prefix('{').and_then(|s| s.strip_suffix('}')).unwrap_or(src);
        let steps = parse_steps(src)?;
        Ok(Self { steps })
    }

    /// All values the path selects in `obj` (missing keys select nothing).
    pub fn find<'a>(&self, obj: &'a Value) -> Vec<&'a Value> {
        let mut cur = vec![obj];
        for st in &self.steps {
            let mut next = vec![];
            for v in cur {
                apply(st, v, &mut next);
            }
            cur = next;
        }
        cur
    }
}

fn apply<'a>(st: &Step, v: &'a Value, out: &mut Vec<&'a Value>) {
    match st {
        Step::Field(f) => {
            if let Some(x) = v.get(f.as_str()) {
                out.push(x);
            }
        }
        Step::Index(i) => {
            if let Some(a) = v.as_array() {
                let idx = if *i < 0 { a.len() as i64 + i } else { *i };
                if idx >= 0 {
                    if let Some(x) = a.get(idx as usize) {
                        out.push(x);
                    }
                }
            }
        }
        Step::All => match v {
            Value::Array(a) => out.extend(a.iter()),
            Value::Object(m) => out.extend(m.values()),
            _ => {}
        },
        Step::Filter { path, negate, literal } => {
            if let Some(a) = v.as_array() {
                for item in a {
                    let mut cur = vec![item];
                    for st in path {
                        let mut n = vec![];
                        for x in cur {
                            apply(st, x, &mut n);
                        }
                        cur = n;
                    }
                    let hit = cur.first().is_some_and(|x| eq(x, literal));
                    if hit != *negate && (!cur.is_empty() || *negate) {
                        out.push(item);
                    }
                }
            }
        }
    }
}

fn eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

fn parse_steps(src: &str) -> Result<Vec<Step>, String> {
    let b: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut steps = vec![];
    if b.first() == Some(&'$') {
        i = 1;
    }
    while i < b.len() {
        match b[i] {
            '.' => {
                i += 1;
                if i < b.len() && b[i] == '*' {
                    steps.push(Step::All);
                    i += 1;
                    continue;
                }
                let start = i;
                while i < b.len() && b[i] != '.' && b[i] != '[' {
                    i += 1;
                }
                let name: String = b[start..i].iter().collect();
                if !name.is_empty() {
                    steps.push(Step::Field(name));
                }
            }
            '[' => {
                let end = find_close(&b, i)?;
                let inner: String = b[i + 1..end].iter().collect();
                steps.push(parse_bracket(inner.trim())?);
                i = end + 1;
            }
            '@' if steps.is_empty() => i += 1,
            c => return Err(format!("unrecognized character in action: {c:?}")),
        }
    }
    Ok(steps)
}

fn find_close(b: &[char], open: usize) -> Result<usize, String> {
    let (mut depth, mut quote) = (0, None::<char>);
    for (j, c) in b.iter().enumerate().skip(open) {
        match (quote, c) {
            (Some(q), c) if *c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(*c),
            (None, '[') => depth += 1,
            (None, ']') => {
                depth -= 1;
                if depth == 0 {
                    return Ok(j);
                }
            }
            _ => {}
        }
    }
    Err("unclosed array expect ]".into())
}

fn parse_bracket(inner: &str) -> Result<Step, String> {
    if inner == "*" {
        return Ok(Step::All);
    }
    if let Some(q) = inner.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return Ok(Step::Field(q.to_string()));
    }
    if let Some(q) = inner.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return Ok(Step::Field(q.to_string()));
    }
    if let Some(f) = inner.strip_prefix("?(").and_then(|s| s.strip_suffix(')')) {
        let (lhs, rhs, negate) = if let Some((l, r)) = f.split_once("!=") {
            (l, r, true)
        } else if let Some((l, r)) = f.split_once("==") {
            (l, r, false)
        } else {
            return Err(format!("unsupported filter {f:?}"));
        };
        let lhs = lhs.trim();
        let path = parse_steps(lhs.strip_prefix('@').ok_or_else(|| format!("filter must start with @: {lhs:?}"))?)?;
        let rhs = rhs.trim();
        let literal = if let Some(q) = rhs.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
            Value::String(q.into())
        } else if let Some(q) = rhs.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
            Value::String(q.into())
        } else {
            serde_json::from_str(rhs).map_err(|_| format!("invalid filter literal {rhs:?}"))?
        };
        return Ok(Step::Filter { path, negate, literal });
    }
    inner.parse::<i64>().map(Step::Index).map_err(|_| format!("invalid array index {inner:?}"))
}

/// How `jsonpath.PrintResults` renders one value (strings bare, others as JSON).
pub fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn paths() {
        let o = json!({"spec": {"replicas": 3, "list": [{"n": "a"}, {"n": "b"}]},
            "metadata": {"labels": {"app.kubernetes.io/name": "x"}},
            "status": {"conditions": [{"type": "Synced", "status": "False"}, {"type": "Ready", "status": "True"}]}});
        let f = |p: &str| JsonPath::parse(p).unwrap().find(&o).into_iter().cloned().collect::<Vec<_>>();
        assert_eq!(f(".spec.replicas"), vec![json!(3)]);
        assert_eq!(f(".spec.list[1].n"), vec![json!("b")]);
        assert_eq!(f(".spec.list[*].n"), vec![json!("a"), json!("b")]);
        assert_eq!(f(".status.conditions[?(@.type==\"Ready\")].status"), vec![json!("True")]);
        assert_eq!(f(".metadata.labels['app.kubernetes.io/name']"), vec![json!("x")]);
        assert!(f(".spec.missing.x").is_empty());
        assert!(JsonPath::parse(".spec[").is_err());
    }
}
