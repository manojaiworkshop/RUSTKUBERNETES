//! Port of `k8s.io/apimachinery/pkg/util/validation` and
//! `k8s.io/apimachinery/pkg/util/validation/field`.

use std::collections::BTreeMap;
use std::fmt;

/// Kind of a field validation error (`field.ErrorType`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Required,
    Invalid(String),
    Duplicate(String),
    NotSupported { value: String, supported: Vec<String> },
    Forbidden(String),
    TooLong(usize),
}

impl ErrorKind {
    pub fn reason(&self) -> &'static str {
        match self {
            ErrorKind::Required => "FieldValueRequired",
            ErrorKind::Invalid(_) => "FieldValueInvalid",
            ErrorKind::Duplicate(_) => "FieldValueDuplicate",
            ErrorKind::NotSupported { .. } => "FieldValueNotSupported",
            ErrorKind::Forbidden(_) => "FieldValueForbidden",
            ErrorKind::TooLong(_) => "FieldValueTooLong",
        }
    }
}

/// A single field error (`field.Error`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldError {
    pub field: String,
    pub kind: ErrorKind,
    pub detail: String,
}

impl FieldError {
    pub fn required(field: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { field: field.into(), kind: ErrorKind::Required, detail: detail.into() }
    }

    pub fn invalid(field: impl Into<String>, value: impl fmt::Display, detail: impl Into<String>) -> Self {
        Self { field: field.into(), kind: ErrorKind::Invalid(value.to_string()), detail: detail.into() }
    }

    pub fn duplicate(field: impl Into<String>, value: impl fmt::Display) -> Self {
        Self { field: field.into(), kind: ErrorKind::Duplicate(value.to_string()), detail: String::new() }
    }

    pub fn not_supported(field: impl Into<String>, value: impl fmt::Display, supported: &[&str]) -> Self {
        Self {
            field: field.into(),
            kind: ErrorKind::NotSupported { value: value.to_string(), supported: supported.iter().map(|s| s.to_string()).collect() },
            detail: String::new(),
        }
    }

    pub fn forbidden(field: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { field: field.into(), kind: ErrorKind::Forbidden(String::new()), detail: detail.into() }
    }

    pub fn too_long(field: impl Into<String>, max: usize) -> Self {
        Self { field: field.into(), kind: ErrorKind::TooLong(max), detail: String::new() }
    }

    /// The message without the field path (used in `StatusCause.message`).
    pub fn message(&self) -> String {
        let base = match &self.kind {
            ErrorKind::Required => "Required value".to_string(),
            ErrorKind::Invalid(v) => format!("Invalid value: {v:?}"),
            ErrorKind::Duplicate(v) => format!("Duplicate value: {v:?}"),
            ErrorKind::NotSupported { value, supported } => format!(
                "Unsupported value: {value:?}: supported values: {}",
                supported.iter().map(|s| format!("{s:?}")).collect::<Vec<_>>().join(", ")
            ),
            ErrorKind::Forbidden(_) => "Forbidden".to_string(),
            ErrorKind::TooLong(max) => format!("Too long: may not be more than {max} bytes"),
        };
        if self.detail.is_empty() {
            base
        } else {
            format!("{base}: {}", self.detail)
        }
    }
}

impl fmt::Display for FieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.message())
    }
}

pub type ErrorList = Vec<FieldError>;

pub const DNS1123_LABEL_MAX_LEN: usize = 63;
pub const DNS1123_SUBDOMAIN_MAX_LEN: usize = 253;

fn is_alnum_lower(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit()
}

/// RFC 1123 label: lowercase alphanumerics or '-', starts and ends alphanumeric, ≤ 63 chars.
pub fn is_dns1123_label(s: &str) -> Vec<String> {
    let mut errs = vec![];
    if s.len() > DNS1123_LABEL_MAX_LEN {
        errs.push(format!("must be no more than {DNS1123_LABEL_MAX_LEN} characters"));
    }
    let ok =
        !s.is_empty() && s.chars().all(|c| is_alnum_lower(c) || c == '-') && s.starts_with(is_alnum_lower) && s.ends_with(is_alnum_lower);
    if !ok {
        errs.push("a lowercase RFC 1123 label must consist of lower case alphanumeric characters or '-', and must start and end with an alphanumeric character (e.g. 'my-name',  or '123-abc', regex used for validation is '[a-z0-9]([-a-z0-9]*[a-z0-9])?')".into());
    }
    errs
}

/// RFC 1123 subdomain: dot-separated labels, ≤ 253 chars.
pub fn is_dns1123_subdomain(s: &str) -> Vec<String> {
    let mut errs = vec![];
    if s.len() > DNS1123_SUBDOMAIN_MAX_LEN {
        errs.push(format!("must be no more than {DNS1123_SUBDOMAIN_MAX_LEN} characters"));
    }
    let ok = !s.is_empty()
        && s.chars().all(|c| is_alnum_lower(c) || c == '-' || c == '.')
        && s.starts_with(is_alnum_lower)
        && s.ends_with(is_alnum_lower)
        && s.split('.').all(|p| !p.is_empty() && p.starts_with(is_alnum_lower) && p.ends_with(is_alnum_lower));
    if !ok {
        errs.push("a lowercase RFC 1123 subdomain must consist of lower case alphanumeric characters, '-' or '.', and must start and end with an alphanumeric character (e.g. 'example.com', regex used for validation is '[a-z0-9]([-a-z0-9]*[a-z0-9])?(\\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*')".into());
    }
    errs
}

/// RFC 1035 label (used for Service names): like a DNS-1123 label but must start with a letter.
pub fn is_dns1035_label(s: &str) -> Vec<String> {
    let mut errs = is_dns1123_label(s);
    if errs.is_empty() && !s.starts_with(|c: char| c.is_ascii_lowercase()) {
        errs.push("a DNS-1035 label must consist of lower case alphanumeric characters or '-', start with an alphabetic character, and end with an alphanumeric character".into());
    }
    errs
}

/// Qualified name: `[prefix/]name` where name is ≤63 chars of `[A-Za-z0-9_.-]`
/// starting and ending alphanumeric, prefix is a DNS-1123 subdomain.
pub fn is_qualified_name(s: &str) -> Vec<String> {
    let (prefix, name) = match s.split_once('/') {
        Some((p, n)) => (Some(p), n),
        None => (None, s),
    };
    let mut errs = vec![];
    if let Some(p) = prefix {
        if p.is_empty() {
            errs.push("prefix part must be non-empty".into());
        } else {
            errs.extend(is_dns1123_subdomain(p).into_iter().map(|e| format!("prefix part {e}")));
        }
    }
    if name.is_empty() {
        errs.push("name part must be non-empty".into());
    } else if name.len() > 63 {
        errs.push("name part must be no more than 63 characters".into());
    } else if !(name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name.ends_with(|c: char| c.is_ascii_alphanumeric()))
    {
        errs.push(
            "name part must consist of alphanumeric characters, '-', '_' or '.', and must start and end with an alphanumeric character"
                .into(),
        );
    }
    errs
}

/// Label value: empty, or ≤63 chars of `[A-Za-z0-9_.-]` starting/ending alphanumeric.
pub fn is_valid_label_value(s: &str) -> Vec<String> {
    if s.is_empty() {
        return vec![];
    }
    let mut errs = vec![];
    if s.len() > 63 {
        errs.push("must be no more than 63 characters".into());
    }
    if !(s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && s.starts_with(|c: char| c.is_ascii_alphanumeric())
        && s.ends_with(|c: char| c.is_ascii_alphanumeric()))
    {
        errs.push("a valid label must be an empty string or consist of alphanumeric characters, '-', '_' or '.', and must start and end with an alphanumeric character".into());
    }
    errs
}

/// C identifier (`[A-Za-z_][A-Za-z0-9_]*`), used for env var names.
pub fn is_c_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn is_valid_port_num(p: i64) -> bool {
    (1..=65535).contains(&p)
}

/// Validates a label map (`metadata.labels`, selectors).
pub fn validate_labels(labels: &BTreeMap<String, String>, path: &str) -> ErrorList {
    let mut errs = vec![];
    for (k, v) in labels {
        for msg in is_qualified_name(k) {
            errs.push(FieldError::invalid(path, k, msg));
        }
        for msg in is_valid_label_value(v) {
            errs.push(FieldError::invalid(format!("{path}[{k}]"), v, msg));
        }
    }
    errs
}

/// Validates annotation keys and total size (256 KiB).
pub fn validate_annotations(annotations: &BTreeMap<String, String>, path: &str) -> ErrorList {
    let mut errs = vec![];
    let mut total = 0usize;
    for (k, v) in annotations {
        for msg in is_qualified_name(&k.to_lowercase()) {
            errs.push(FieldError::invalid(path, k, msg));
        }
        total += k.len() + v.len();
    }
    if total > 256 * 1024 {
        errs.push(FieldError::too_long(path, 256 * 1024));
    }
    errs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_labels() {
        assert!(is_dns1123_label("my-name").is_empty());
        assert!(is_dns1123_label("123-abc").is_empty());
        assert!(!is_dns1123_label("My-Name").is_empty());
        assert!(!is_dns1123_label("-abc").is_empty());
        assert!(!is_dns1123_label(&"a".repeat(64)).is_empty());
        assert!(is_dns1123_subdomain("example.com").is_empty());
        assert!(!is_dns1123_subdomain("example..com").is_empty());
        assert!(!is_dns1035_label("1abc").is_empty());
    }

    #[test]
    fn qualified_names() {
        assert!(is_qualified_name("app").is_empty());
        assert!(is_qualified_name("kubernetes.io/hostname").is_empty());
        assert!(!is_qualified_name("/x").is_empty());
        assert!(!is_qualified_name("a/b/c").is_empty());
        assert!(is_valid_label_value("").is_empty());
        assert!(!is_valid_label_value("-x").is_empty());
    }
}
