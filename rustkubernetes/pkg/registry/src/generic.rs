//! A table-driven [`Strategy`] for resources without side effects.

use async_trait::async_trait;
use k8s_apimachinery::validation::ErrorList;
use k8s_apiserver::table::{age_cell, Column};
use k8s_apiserver::Strategy;
use serde_json::Value;

pub type DefaultsFn = fn(&mut Value);
pub type ValidateFn = fn(&Value) -> ErrorList;
pub type ValidateUpdateFn = fn(&Value, &Value) -> ErrorList;
pub type ColumnsFn = fn() -> Vec<Column>;
pub type CellsFn = fn(&Value) -> Vec<Value>;
pub type NameFn = fn(&str) -> Vec<String>;

pub struct SimpleStrategy {
    pub has_status: bool,
    /// `/status` may change labels and annotations (nodes).
    pub status_metadata: bool,
    /// Name validation other than DNS-1123 subdomain.
    pub name: Option<NameFn>,
    pub defaults: Option<DefaultsFn>,
    pub validate: Option<ValidateFn>,
    pub validate_update: Option<ValidateUpdateFn>,
    pub columns: Option<ColumnsFn>,
    pub cells: Option<CellsFn>,
}

impl SimpleStrategy {
    pub fn new(has_status: bool) -> Self {
        Self {
            has_status,
            status_metadata: false,
            name: None,
            defaults: None,
            validate: None,
            validate_update: None,
            columns: None,
            cells: None,
        }
    }

    pub fn defaults(mut self, f: DefaultsFn) -> Self {
        self.defaults = Some(f);
        self
    }

    pub fn validate(mut self, f: ValidateFn) -> Self {
        self.validate = Some(f);
        self
    }

    pub fn validate_update(mut self, f: ValidateUpdateFn) -> Self {
        self.validate_update = Some(f);
        self
    }

    pub fn name_validation(mut self, f: NameFn) -> Self {
        self.name = Some(f);
        self
    }

    pub fn status_updates_metadata(mut self) -> Self {
        self.status_metadata = true;
        self
    }

    pub fn table(mut self, columns: ColumnsFn, cells: CellsFn) -> Self {
        self.columns = Some(columns);
        self.cells = Some(cells);
        self
    }
}

#[async_trait]
impl Strategy for SimpleStrategy {
    fn has_status(&self) -> bool {
        self.has_status
    }

    fn status_updates_metadata(&self) -> bool {
        self.status_metadata
    }

    fn validate_name(&self, name: &str) -> Vec<String> {
        match self.name {
            Some(f) => f(name),
            None => k8s_apimachinery::validation::is_dns1123_subdomain(name),
        }
    }

    fn prepare_for_create(&self, obj: &mut Value) {
        if let Some(d) = self.defaults {
            d(obj)
        }
    }

    fn prepare_for_update(&self, new: &mut Value, _old: &Value) {
        if let Some(d) = self.defaults {
            d(new)
        }
    }

    fn validate(&self, obj: &Value) -> ErrorList {
        self.validate.map(|f| f(obj)).unwrap_or_default()
    }

    fn validate_update(&self, new: &Value, old: &Value) -> ErrorList {
        match (self.validate_update, self.validate) {
            (Some(f), _) => f(new, old),
            (None, Some(f)) => f(new),
            _ => vec![],
        }
    }

    fn table_columns(&self) -> Vec<Column> {
        self.columns.map(|f| f()).unwrap_or_else(|| vec![Column::new("Age", "string", "")])
    }

    fn table_cells(&self, obj: &Value) -> Vec<Value> {
        self.cells.map(|f| f(obj)).unwrap_or_else(|| vec![age_cell(obj)])
    }
}

/// Reads an integer at a JSON pointer, defaulting to 0.
pub fn int_at(obj: &Value, ptr: &str) -> i64 {
    obj.pointer(ptr).and_then(Value::as_i64).unwrap_or(0)
}

/// Reads a string at a JSON pointer, defaulting to "".
pub fn str_at<'a>(obj: &'a Value, ptr: &str) -> &'a str {
    obj.pointer(ptr).and_then(Value::as_str).unwrap_or("")
}

/// `a=b,c=d` from a label map value.
pub fn format_labels(v: Option<&Value>) -> String {
    v.and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or(""))).collect::<Vec<_>>().join(","))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "<none>".into())
}
