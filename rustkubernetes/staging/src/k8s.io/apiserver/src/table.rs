//! Server-side table printing (`meta.k8s.io/v1 Table`), used by `kubectl get`.
//!
//! When a client sends `Accept: application/json;as=Table;g=meta.k8s.io;v=v1`
//! the server answers with column definitions and one row per object, so the
//! client does not need to know how to print each type.

use k8s_apimachinery::meta::human_age;
use k8s_apimachinery::unstructured::ObjectExt;
use serde_json::{json, Value};

#[derive(Clone, Debug)]
pub struct Column {
    pub name: String,
    pub type_: String,
    pub format: String,
    pub description: String,
    /// 0 = always shown, 1 = only with `-o wide`.
    pub priority: i32,
}

impl Column {
    pub fn new(name: &str, type_: &str, description: &str) -> Self {
        Self { name: name.into(), type_: type_.into(), format: String::new(), description: description.into(), priority: 0 }
    }

    pub fn wide(mut self) -> Self {
        self.priority = 1;
        self
    }

    fn to_json(&self) -> Value {
        json!({
            "name": self.name, "type": self.type_, "format": self.format,
            "description": self.description, "priority": self.priority,
        })
    }
}

/// The `AGE` cell for an object.
pub fn age_cell(obj: &Value) -> Value {
    Value::String(human_age(obj.creation_timestamp()))
}

/// Which object is embedded in each row (`?includeObject=`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IncludeObject {
    None,
    Metadata,
    Object,
}

impl IncludeObject {
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("None") => IncludeObject::None,
            Some("Object") => IncludeObject::Object,
            _ => IncludeObject::Metadata,
        }
    }
}

fn row_object(obj: &Value, include: IncludeObject) -> Option<Value> {
    match include {
        IncludeObject::None => None,
        IncludeObject::Object => Some(obj.clone()),
        IncludeObject::Metadata => Some(json!({
            "kind": "PartialObjectMetadata",
            "apiVersion": "meta.k8s.io/v1",
            "metadata": obj.get("metadata").cloned().unwrap_or(json!({})),
        })),
    }
}

/// Builds a Table from objects. `columns`/`cells` come from the resource's strategy.
pub fn build_table(
    columns: &[Column],
    objects: &[Value],
    cells: impl Fn(&Value) -> Vec<Value>,
    include: IncludeObject,
    resource_version: &str,
    continue_token: Option<&str>,
    with_column_definitions: bool,
) -> Value {
    let mut defs = vec![Column {
        name: "Name".into(),
        type_: "string".into(),
        format: "name".into(),
        description: "Name must be unique within a namespace.".into(),
        priority: 0,
    }
    .to_json()];
    defs.extend(columns.iter().map(Column::to_json));
    let rows: Vec<Value> = objects
        .iter()
        .map(|o| {
            // A printer may supply the Name cell itself (one cell more than its columns),
            // e.g. `standard (default)` for StorageClasses.
            let own = cells(o);
            let mut c = if own.len() == columns.len() + 1 { vec![] } else { vec![Value::String(o.name().to_string())] };
            c.extend(own);
            let mut row = json!({ "cells": c });
            if let Some(obj) = row_object(o, include) {
                row["object"] = obj;
            }
            row
        })
        .collect();
    let mut meta = json!({ "resourceVersion": resource_version });
    if let Some(c) = continue_token {
        meta["continue"] = Value::String(c.into());
    }
    let mut t = json!({
        "kind": "Table",
        "apiVersion": "meta.k8s.io/v1",
        "metadata": meta,
        "rows": rows,
    });
    if with_column_definitions {
        t["columnDefinitions"] = Value::Array(defs);
    } else {
        t["columnDefinitions"] = json!([]);
    }
    t
}

/// True if the `Accept` header asks for a Table.
pub fn wants_table(accept: &str) -> bool {
    accept.split(',').next().is_some_and(|first| first.contains("as=Table"))
}
