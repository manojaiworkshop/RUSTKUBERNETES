//! Port of `apiextensions-apiserver/pkg/apiserver/schema` and the
//! `k8s.io/kube-openapi/pkg/validation/validate` schema validator, working on
//! the CRD's `openAPIV3Schema` (a `JSONSchemaProps` JSON object):
//!
//! | Function | Upstream |
//! |----------|----------|
//! | [`validate`] | `validation.ValidateCustomResource` → `SchemaValidator.Validate`, errors converted by `kubeOpenAPIResultToFieldErrors` |
//! | [`prune`] | `schema/pruning.PruneWithOptions` (unknown field paths for `fieldValidation`) |
//! | [`default`] | `schema/defaulting.Default` |
//! | [`prune_non_nullable_nulls`] | `defaulting.PruneNonNullableNullsWithoutDefaults` |
//! | [`coerce_root_metadata`] | `schema/objectmeta.GetObjectMetaWithOptions` (unknown `metadata.*` fields) |
//! | [`validate_list_sets_and_maps`] | `schema/listtype.ValidateListSetsAndMaps` |
//!
//! `x-kubernetes-validations` (CEL) rules are not evaluated.

use std::collections::BTreeSet;

use base64::Engine;
use k8s_apimachinery::validation::{ErrorList, FieldError};
use serde_json::{Map, Value};

fn s<'a>(schema: &'a Value, k: &str) -> &'a str {
    schema.get(k).and_then(Value::as_str).unwrap_or("")
}

fn flag(schema: &Value, k: &str) -> bool {
    schema.get(k).and_then(Value::as_bool).unwrap_or(false)
}

fn props(schema: &Value) -> Option<&Map<String, Value>> {
    schema.get("properties").and_then(Value::as_object)
}

/// The schema of `additionalProperties` when it is a schema (not a bool).
fn additional(schema: &Value) -> Option<&Value> {
    schema.get("additionalProperties").filter(|a| a.is_object())
}

fn items(schema: &Value) -> Option<&Value> {
    schema.get("items").filter(|a| a.is_object())
}

pub fn preserves_unknown(schema: &Value) -> bool {
    flag(schema, "x-kubernetes-preserve-unknown-fields")
}

fn child(path: &str, k: &str) -> String {
    if path.is_empty() {
        k.to_string()
    } else {
        format!("{path}.{k}")
    }
}

// ---------------------------------------------------------------------------
// validation
// ---------------------------------------------------------------------------

/// One kube-openapi validation error before conversion to a field error.
enum Fail {
    Required,
    Enum(Value, Vec<Value>),
    TooLong(i64),
    TooShort(i64),
    TooMany(i64, i64),
    TooFew(i64, i64),
    InvalidType(Value, String),
    Other(Value, String),
    /// Composite (oneOf/anyOf/allOf/not) errors are reported on the root path.
    Composite(String),
}

struct Validator {
    errs: Vec<(String, Fail)>,
}

/// Validates `obj` against `schema` (kube-openapi semantics), returning field errors
/// shaped like upstream's (`spec.replicas: Invalid value: "string": spec.replicas in
/// body must be of type integer: "string"`).
pub fn validate(schema: &Value, obj: &Value) -> ErrorList {
    let mut v = Validator { errs: vec![] };
    v.validate(schema, obj, "");
    v.errs
        .into_iter()
        .map(|(name, f)| {
            let path = name.trim_start_matches('.').to_string();
            match f {
                Fail::Required => FieldError::required(path, ""),
                Fail::Enum(val, allowed) => FieldError::not_supported_json(
                    path,
                    &val,
                    allowed.iter().map(|a| a.as_str().map(String::from).unwrap_or_else(|| a.to_string())).collect(),
                ),
                Fail::TooLong(max) => FieldError::too_long(path, max.max(0) as usize),
                Fail::TooShort(min) => FieldError::too_short(path, min),
                Fail::TooMany(actual, max) => FieldError::too_many(path, actual, max),
                Fail::TooFew(actual, min) => FieldError::too_few(path, actual, min),
                Fail::InvalidType(val, msg) => FieldError::type_invalid(path, &val, msg),
                Fail::Other(val, msg) => FieldError::invalid_json(path, &val, msg),
                Fail::Composite(msg) => FieldError::invalid("", "", msg),
            }
        })
        .collect()
}

/// `%v` of a float64 the way Go prints schema bounds (`10`, `1.5`).
fn num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e21 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn types(schema: &Value) -> Vec<&str> {
    if flag(schema, "x-kubernetes-int-or-string") {
        return vec!["integer", "string"];
    }
    match s(schema, "type") {
        "" => vec![],
        t => vec![t],
    }
}

/// The schema type of a JSON value (`typeValidator.schemaInfoForType`).
fn data_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::String(_) => "string",
        Value::Number(n) if n.is_f64() => "number",
        Value::Number(_) => "integer",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(a, b)| json_eq(a, b)),
        (Value::Object(x), Value::Object(y)) => x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| json_eq(v, w))),
        _ => a == b,
    }
}

impl Validator {
    fn err(&mut self, path: &str, f: Fail) {
        self.errs.push((path.to_string(), f));
    }

    fn ok(schema: &Value, data: &Value, path: &str) -> bool {
        let mut v = Validator { errs: vec![] };
        v.validate(schema, data, path);
        v.errs.is_empty()
    }

    fn validate(&mut self, schema: &Value, data: &Value, path: &str) {
        let tys = types(schema);
        // type validator
        if data.is_null() {
            if !tys.is_empty() && !flag(schema, "nullable") && !tys.contains(&"null") {
                self.err(
                    path,
                    Fail::InvalidType(Value::String("null".into()), format!("{path} in body must be of type {}: \"null\"", tys.join(","))),
                );
            }
            self.validate_enum(schema, data, path);
            return;
        }
        let dt = data_type(data);
        let float_int = dt == "number" && data.as_f64().is_some_and(|f| f.fract() == 0.0) && tys.contains(&"integer");
        let int_float = dt == "integer" && tys.contains(&"number");
        let type_ok = tys.is_empty() || tys.contains(&dt) || float_int || int_float;
        if !type_ok {
            self.err(
                path,
                Fail::InvalidType(Value::String(dt.into()), format!("{path} in body must be of type {}: {dt:?}", tys.join(","))),
            );
        }
        self.composition(schema, data, path);
        match data {
            Value::String(st) => self.validate_string(schema, st, path),
            Value::Number(_) => self.validate_number(schema, data, path),
            Value::Array(a) => self.validate_slice(schema, a, path),
            Value::Object(o) => self.validate_object(schema, o, path),
            _ => {}
        }
        self.validate_enum(schema, data, path);
    }

    fn composition(&mut self, schema: &Value, data: &Value, path: &str) {
        let list = |k: &str| schema.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
        let any_of = list("anyOf");
        if !any_of.is_empty() && !any_of.iter().any(|alt| Self::ok(alt, data, path)) {
            self.err(path, Fail::Composite(format!("{path:?} must validate at least one schema (anyOf)")));
        }
        let one_of = list("oneOf");
        if !one_of.is_empty() {
            let n = one_of.iter().filter(|alt| Self::ok(alt, data, path)).count();
            if n != 1 {
                let extra = if n == 0 { "Found none valid".to_string() } else { format!("Found {n} valid alternatives") };
                self.err(path, Fail::Composite(format!("{path:?} must validate one and only one schema (oneOf). {extra}")));
            }
        }
        let all_of = list("allOf");
        if !all_of.is_empty() {
            let mut validated = 0;
            for alt in &all_of {
                let before = self.errs.len();
                self.validate(alt, data, path);
                if self.errs.len() == before {
                    validated += 1;
                }
            }
            if validated != all_of.len() {
                let extra = if validated == 0 { ". None validated" } else { "" };
                self.err(path, Fail::Composite(format!("{path:?} must validate all the schemas (allOf){extra}")));
            }
        }
        if let Some(not) = schema.get("not").filter(|n| n.is_object()) {
            if Self::ok(not, data, path) {
                self.err(path, Fail::Composite(format!("{path:?} must not validate the schema (not)")));
            }
        }
    }

    fn validate_string(&mut self, schema: &Value, st: &str, path: &str) {
        let len = st.chars().count() as i64;
        if let Some(max) = schema.get("maxLength").and_then(Value::as_i64) {
            if len > max {
                self.err(path, Fail::TooLong(max));
            }
        }
        if let Some(min) = schema.get("minLength").and_then(Value::as_i64) {
            if len < min {
                self.err(path, Fail::TooShort(min));
            }
        }
        if let Some(p) = schema.get("pattern").and_then(Value::as_str) {
            if let Ok(re) = regex::Regex::new(p) {
                if !re.is_match(st) {
                    self.err(path, Fail::Other(Value::String(st.into()), format!("{path} in body should match '{p}'")));
                }
            }
        }
        let format = s(schema, "format");
        if !format.is_empty() && types(schema).contains(&"string") && !check_format(format, st) {
            self.err(path, Fail::InvalidType(Value::String(st.into()), format!("{path} in body must be of type {format}: {st:?}")));
        }
    }

    fn validate_number(&mut self, schema: &Value, data: &Value, path: &str) {
        let x = data.as_f64().unwrap_or(0.0);
        if let Some(m) = schema.get("multipleOf").and_then(Value::as_f64) {
            if m > 0.0 && (x / m).fract() != 0.0 {
                self.err(path, Fail::Other(data.clone(), format!("{path} in body should be a multiple of {}", num(m))));
            }
        }
        if let Some(max) = schema.get("maximum").and_then(Value::as_f64) {
            if flag(schema, "exclusiveMaximum") {
                if x >= max {
                    self.err(path, Fail::Other(data.clone(), format!("{path} in body should be less than {}", num(max))));
                }
            } else if x > max {
                self.err(path, Fail::Other(data.clone(), format!("{path} in body should be less than or equal to {}", num(max))));
            }
        }
        if let Some(min) = schema.get("minimum").and_then(Value::as_f64) {
            if flag(schema, "exclusiveMinimum") {
                if x <= min {
                    self.err(path, Fail::Other(data.clone(), format!("{path} in body should be greater than {}", num(min))));
                }
            } else if x < min {
                self.err(path, Fail::Other(data.clone(), format!("{path} in body should be greater than or equal to {}", num(min))));
            }
        }
    }

    fn validate_slice(&mut self, schema: &Value, a: &[Value], path: &str) {
        if let Some(it) = items(schema) {
            for (i, x) in a.iter().enumerate() {
                self.validate(it, x, &format!("{path}[{i}]"));
            }
        }
        let n = a.len() as i64;
        if let Some(min) = schema.get("minItems").and_then(Value::as_i64) {
            if n < min {
                self.err(path, Fail::TooFew(n, min));
            }
        }
        if let Some(max) = schema.get("maxItems").and_then(Value::as_i64) {
            if n > max {
                self.err(path, Fail::TooMany(n, max));
            }
        }
        if flag(schema, "uniqueItems") && a.iter().enumerate().any(|(i, x)| a[..i].iter().any(|y| json_eq(x, y))) {
            self.err(path, Fail::Other(Value::String(String::new()), format!("{path} in body shouldn't contain duplicates")));
        }
    }

    fn validate_object(&mut self, schema: &Value, o: &Map<String, Value>, path: &str) {
        let n = o.len() as i64;
        if let Some(max) = schema.get("maxProperties").and_then(Value::as_i64) {
            if n > max {
                self.err(path, Fail::TooMany(n, max));
            }
        }
        if let Some(min) = schema.get("minProperties").and_then(Value::as_i64) {
            if n < min {
                self.err(path, Fail::TooFew(n, min));
            }
        }
        let p = props(schema);
        if let Some(p) = p {
            for (k, ps) in p {
                if let Some(v) = o.get(k) {
                    self.validate(ps, v, &child(path, k));
                }
            }
        }
        if let Some(req) = schema.get("required").and_then(Value::as_array) {
            for k in req.iter().filter_map(Value::as_str) {
                if !o.contains_key(k) {
                    self.err(&format!("{path}.{k}"), Fail::Required);
                }
            }
        }
        if let Some(ap) = additional(schema) {
            for (k, v) in o {
                if p.is_some_and(|p| p.contains_key(k)) {
                    continue;
                }
                self.validate(ap, v, &format!("{path}.{k}"));
            }
        }
    }

    fn validate_enum(&mut self, schema: &Value, data: &Value, path: &str) {
        if let Some(e) = schema.get("enum").and_then(Value::as_array) {
            if !e.is_empty() && !e.iter().any(|x| json_eq(x, data)) {
                self.err(path, Fail::Enum(data.clone(), e.clone()));
            }
        }
    }
}

/// The string formats kube-openapi checks (others are accepted as is).
fn check_format(format: &str, v: &str) -> bool {
    match format {
        "byte" => base64::engine::general_purpose::STANDARD.decode(v).is_ok(),
        "date-time" | "datetime" => chrono::DateTime::parse_from_rfc3339(v).is_ok(),
        "date" => chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d").is_ok(),
        "ipv4" => v.parse::<std::net::Ipv4Addr>().is_ok(),
        "ipv6" => v.parse::<std::net::Ipv6Addr>().is_ok(),
        "cidr" => v.split_once('/').is_some_and(|(ip, bits)| {
            ip.parse::<std::net::IpAddr>().is_ok_and(|ip| bits.parse::<u8>().is_ok_and(|b| b <= if ip.is_ipv4() { 32 } else { 128 }))
        }),
        "uuid" | "uuid3" | "uuid4" | "uuid5" => {
            let parts: Vec<&str> = v.split('-').collect();
            parts.len() == 5 && [8, 4, 4, 4, 12].iter().zip(&parts).all(|(n, p)| p.len() == *n && p.chars().all(|c| c.is_ascii_hexdigit()))
        }
        "mac" => {
            let parts: Vec<&str> = v.split([':', '-']).collect();
            matches!(parts.len(), 6 | 8 | 20) && parts.iter().all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
        }
        "hostname" => {
            !v.is_empty()
                && v.len() <= 255
                && v.split('.').all(|l| {
                    !l.is_empty()
                        && l.len() <= 63
                        && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                        && !l.starts_with('-')
                        && !l.ends_with('-')
                })
        }
        "email" => v.split_once('@').is_some_and(|(a, b)| !a.is_empty() && b.contains('.') && !b.starts_with('.')),
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// pruning & defaulting
// ---------------------------------------------------------------------------

const META_FIELDS: &[&str] = &["apiVersion", "kind", "metadata"];

/// Drops fields the schema does not specify and returns their paths (sorted),
/// e.g. `spec.foo` or `spec.items[0].bar`. `root` treats the object as a resource
/// (`apiVersion`, `kind` and `metadata` are kept).
pub fn prune(obj: &mut Value, schema: Option<&Value>, root: bool) -> Vec<String> {
    let mut unknown = vec![];
    let embedded_root;
    let schema = match (schema, root) {
        (Some(sc), true) if !flag(sc, "x-kubernetes-embedded-resource") => {
            let mut c = sc.clone();
            c["x-kubernetes-embedded-resource"] = Value::Bool(true);
            embedded_root = c;
            Some(&embedded_root)
        }
        (None, true) => {
            embedded_root = serde_json::json!({"x-kubernetes-embedded-resource": true});
            Some(&embedded_root)
        }
        (sc, _) => sc,
    };
    prune_rec(obj, schema, "", &mut unknown);
    unknown.sort();
    unknown
}

fn prune_rec(x: &mut Value, schema: Option<&Value>, path: &str, unknown: &mut Vec<String>) {
    if let Some(sc) = schema {
        if preserves_unknown(sc) {
            skip_prune(x, sc, path, unknown);
            return;
        }
    }
    match x {
        Value::Object(m) => {
            let Some(sc) = schema else {
                for k in std::mem::take(m).keys() {
                    unknown.push(child(path, k));
                }
                return;
            };
            let embedded = flag(sc, "x-kubernetes-embedded-resource");
            let keys: Vec<String> = m.keys().cloned().collect();
            for k in keys {
                if embedded && META_FIELDS.contains(&k.as_str()) {
                    continue;
                }
                if let Some(ps) = props(sc).and_then(|p| p.get(&k)) {
                    prune_rec(m.get_mut(&k).unwrap(), Some(ps), &child(path, &k), unknown);
                } else if let Some(ap) = sc.get("additionalProperties").filter(|a| a.is_object() || a.as_bool() == Some(true)) {
                    let ap = ap.is_object().then_some(ap);
                    prune_rec(m.get_mut(&k).unwrap(), ap, &child(path, &k), unknown);
                } else {
                    if !META_FIELDS.contains(&k.as_str()) || !path.is_empty() {
                        unknown.push(child(path, &k));
                    }
                    m.remove(&k);
                }
            }
        }
        Value::Array(a) => {
            let it = schema.and_then(items);
            for (i, v) in a.iter_mut().enumerate() {
                prune_rec(v, it, &format!("{path}[{i}]"), unknown);
            }
        }
        _ => {}
    }
}

fn skip_prune(x: &mut Value, sc: &Value, path: &str, unknown: &mut Vec<String>) {
    match x {
        Value::Object(m) => {
            let embedded = flag(sc, "x-kubernetes-embedded-resource");
            for (k, v) in m.iter_mut() {
                if embedded && META_FIELDS.contains(&k.as_str()) {
                    continue;
                }
                if let Some(ps) = props(sc).and_then(|p| p.get(k)) {
                    prune_rec(v, Some(ps), &child(path, k), unknown);
                } else if let Some(ap) = additional(sc) {
                    prune_rec(v, Some(ap), &child(path, k), unknown);
                }
            }
        }
        Value::Array(a) => {
            if let Some(it) = items(sc) {
                for (i, v) in a.iter_mut().enumerate() {
                    skip_prune(v, it, &format!("{path}[{i}]"), unknown);
                }
            }
        }
        _ => {}
    }
}

fn non_nullable_null(x: &Value, sc: &Value) -> bool {
    x.is_null() && !flag(sc, "nullable")
}

/// Fills in `default`s for missing (or null, non-nullable) fields, recursively.
pub fn default(x: &mut Value, schema: &Value) {
    match x {
        Value::Object(m) => {
            if let Some(p) = props(schema) {
                for (k, ps) in p {
                    let Some(d) = ps.get("default") else { continue };
                    if m.get(k).is_none_or(|v| non_nullable_null(v, ps)) {
                        m.insert(k.clone(), d.clone());
                    }
                }
            }
            let ap = additional(schema);
            for (k, v) in m.iter_mut() {
                if let Some(ps) = props(schema).and_then(|p| p.get(k)) {
                    default(v, ps);
                } else if let Some(ap) = ap {
                    if non_nullable_null(v, ap) {
                        if let Some(d) = ap.get("default") {
                            *v = d.clone();
                        }
                    }
                    default(v, ap);
                }
            }
        }
        Value::Array(a) => {
            if let Some(it) = items(schema) {
                for v in a.iter_mut() {
                    if non_nullable_null(v, it) {
                        if let Some(d) = it.get("default") {
                            *v = d.clone();
                        }
                    }
                    default(v, it);
                }
            }
        }
        _ => {}
    }
}

/// Removes `null` values of non-nullable fields without a default.
pub fn prune_non_nullable_nulls(x: &mut Value, schema: &Value) {
    match x {
        Value::Object(m) => {
            let p = props(schema);
            let ap = additional(schema);
            m.retain(|k, v| {
                let sc = p.and_then(|p| p.get(k)).or(ap);
                !(v.is_null() && sc.is_some_and(|sc| !flag(sc, "nullable") && sc.get("default").is_none()))
            });
            for (k, v) in m.iter_mut() {
                if let Some(sc) = p.and_then(|p| p.get(k)).or(ap) {
                    prune_non_nullable_nulls(v, sc);
                }
            }
        }
        Value::Array(a) => {
            if let Some(it) = items(schema) {
                for v in a.iter_mut() {
                    prune_non_nullable_nulls(v, it);
                }
            }
        }
        _ => {}
    }
}

/// `ObjectMeta` JSON fields; anything else under `metadata` is unknown and dropped.
const OBJECT_META_FIELDS: &[&str] = &[
    "name",
    "generateName",
    "namespace",
    "selfLink",
    "uid",
    "resourceVersion",
    "generation",
    "creationTimestamp",
    "deletionTimestamp",
    "deletionGracePeriodSeconds",
    "labels",
    "annotations",
    "ownerReferences",
    "finalizers",
    "managedFields",
];

/// Drops unknown fields under the root `metadata`, returning `metadata.<field>` paths.
pub fn coerce_root_metadata(obj: &mut Value) -> Vec<String> {
    let mut unknown = vec![];
    if let Some(m) = obj.get_mut("metadata").and_then(Value::as_object_mut) {
        let keys: Vec<String> = m.keys().filter(|k| !OBJECT_META_FIELDS.contains(&k.as_str())).cloned().collect();
        for k in keys {
            m.remove(&k);
            unknown.push(format!("metadata.{k}"));
        }
    }
    unknown
}

// ---------------------------------------------------------------------------
// x-kubernetes-list-type
// ---------------------------------------------------------------------------

/// Duplicate items in `x-kubernetes-list-type: set` lists and duplicate keys in `map` lists.
pub fn validate_list_sets_and_maps(obj: &Value, schema: &Value) -> ErrorList {
    let mut errs = vec![];
    list_rec(obj, schema, "", &mut errs);
    errs
}

fn list_rec(x: &Value, schema: &Value, path: &str, errs: &mut ErrorList) {
    match x {
        Value::Object(m) => {
            for (k, v) in m {
                if let Some(ps) = props(schema).and_then(|p| p.get(k)) {
                    list_rec(v, ps, &child(path, k), errs);
                } else if let Some(ap) = additional(schema) {
                    list_rec(v, ap, &child(path, k), errs);
                }
            }
        }
        Value::Array(a) => {
            match s(schema, "x-kubernetes-list-type") {
                "set" => {
                    for (i, v) in a.iter().enumerate() {
                        if a[..i].iter().any(|w| json_eq(v, w)) {
                            errs.push(FieldError::duplicate_json(format!("{path}[{i}]"), v));
                        }
                    }
                }
                "map" => {
                    let keys: Vec<&str> = schema
                        .get("x-kubernetes-list-map-keys")
                        .and_then(Value::as_array)
                        .map(|k| k.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();
                    let mut seen: BTreeSet<String> = BTreeSet::new();
                    for (i, v) in a.iter().enumerate() {
                        let key: Map<String, Value> = keys.iter().filter_map(|k| v.get(*k).map(|x| (k.to_string(), x.clone()))).collect();
                        let key = Value::Object(key);
                        if !seen.insert(key.to_string()) {
                            errs.push(FieldError::duplicate_json(format!("{path}[{i}]"), &key));
                        }
                    }
                }
                _ => {}
            }
            if let Some(it) = items(schema) {
                for (i, v) in a.iter().enumerate() {
                    list_rec(v, it, &format!("{path}[{i}]"), errs);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({"type": "object", "properties": {
            "spec": {"type": "object", "required": ["image"], "properties": {
                "replicas": {"type": "integer", "minimum": 1, "maximum": 10, "default": 1},
                "image": {"type": "string", "pattern": "^[a-z]+$", "maxLength": 8},
                "mode": {"type": "string", "enum": ["a", "b"]},
                "ports": {"type": "array", "maxItems": 2, "items": {"type": "integer"}, "x-kubernetes-list-type": "set"},
                "port": {"x-kubernetes-int-or-string": true},
                "extra": {"type": "object", "x-kubernetes-preserve-unknown-fields": true},
                "labels": {"type": "object", "additionalProperties": {"type": "string"}},
            }},
            "status": {"type": "object", "properties": {"ready": {"type": "boolean"}}},
        }})
    }

    fn msgs(errs: &ErrorList) -> Vec<String> {
        errs.iter().map(|e| e.to_string()).collect()
    }

    #[test]
    fn validation_messages() {
        let sc = schema();
        let obj = json!({"spec": {"replicas": "x", "image": "ABC", "mode": "c", "ports": [1, 2, 3], "port": true}});
        let m = msgs(&validate(&sc, &obj));
        assert!(
            m.contains(&r#"spec.replicas: Invalid value: "string": spec.replicas in body must be of type integer: "string""#.to_string()),
            "{m:?}"
        );
        assert!(m.contains(&r#"spec.image: Invalid value: "ABC": spec.image in body should match '^[a-z]+$'"#.to_string()), "{m:?}");
        assert!(m.contains(&r#"spec.mode: Unsupported value: "c": supported values: "a", "b""#.to_string()), "{m:?}");
        assert!(m.contains(&"spec.ports: Too many: 3: must have at most 2 items".to_string()), "{m:?}");
        assert!(
            m.contains(&r#"spec.port: Invalid value: "boolean": spec.port in body must be of type integer,string: "boolean""#.to_string()),
            "{m:?}"
        );
        let m = msgs(&validate(&sc, &json!({"spec": {"replicas": 11}})));
        assert!(
            m.contains(&"spec.replicas: Invalid value: 11: spec.replicas in body should be less than or equal to 10".to_string()),
            "{m:?}"
        );
        assert!(m.contains(&"spec.image: Required value".to_string()), "{m:?}");
        let m = msgs(&validate(&sc, &json!({"spec": {"image": "abcdefghij", "replicas": 2.0}})));
        assert_eq!(m, vec!["spec.image: Too long: may not be more than 8 bytes".to_string()]);
    }

    #[test]
    fn pruning_and_defaulting() {
        let sc = schema();
        let mut obj = json!({"apiVersion": "x/v1", "kind": "K", "metadata": {"name": "a"},
            "spec": {"image": "a", "foo": 1, "extra": {"anything": {"x": 1}}, "labels": {"a": "b"}}, "bar": true});
        let unknown = prune(&mut obj, Some(&sc), true);
        assert_eq!(unknown, vec!["bar".to_string(), "spec.foo".to_string()]);
        assert_eq!(obj["spec"]["extra"]["anything"]["x"], 1);
        assert_eq!(obj["metadata"]["name"], "a");
        default(&mut obj, &sc);
        assert_eq!(obj["spec"]["replicas"], 1);
        let mut o2 = json!({"spec": {"mode": null}});
        prune_non_nullable_nulls(&mut o2, &sc);
        assert_eq!(o2, json!({"spec": {}}));
    }

    #[test]
    fn list_sets() {
        let sc = schema();
        let errs = validate_list_sets_and_maps(&json!({"spec": {"ports": [1, 2, 1]}}), &sc);
        assert_eq!(msgs(&errs), vec!["spec.ports[2]: Duplicate value: 1".to_string()]);
    }

    #[test]
    fn metadata() {
        let mut o = json!({"metadata": {"name": "a", "bogus": 1}});
        assert_eq!(coerce_root_metadata(&mut o), vec!["metadata.bogus".to_string()]);
        assert_eq!(o, json!({"metadata": {"name": "a"}}));
    }
}
