//! Output printing (`cli-runtime/pkg/printers`): server-side tables, YAML, JSON.

use anyhow::Result;
use k8s_client_go::{Client, ListParams};
use serde_json::Value;

const TABLE_ACCEPT: &str = "application/json;as=Table;v=v1;g=meta.k8s.io,application/json";

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Fetches a `meta.k8s.io/v1 Table` for a collection or a single object.
pub async fn fetch_table(client: &Client, collection: &str, name: Option<&str>, lp: &ListParams) -> Result<Value> {
    let mut path = match name {
        Some(n) => format!("{collection}/{n}"),
        None => collection.to_string(),
    };
    let mut q = vec![];
    if let Some(s) = &lp.label_selector {
        q.push(format!("labelSelector={}", urlencode(s)));
    }
    if let Some(s) = &lp.field_selector {
        q.push(format!("fieldSelector={}", urlencode(s)));
    }
    if !q.is_empty() {
        path = format!("{path}?{}", q.join("&"));
    }
    match client.get_with_accept(&path, TABLE_ACCEPT).await {
        Ok(t) if t["kind"] == "Table" => Ok(t),
        _ => Ok(raw_table(client, &path).await?),
    }
}

/// Fallback when the server cannot produce tables: NAME only.
async fn raw_table(client: &Client, path: &str) -> Result<Value> {
    let v = client.get_json(path).await?;
    let items = if v["items"].is_array() { v["items"].as_array().cloned().unwrap() } else { vec![v] };
    let rows: Vec<Value> =
        items.iter().map(|o| serde_json::json!({"cells": [o["metadata"]["name"]], "object": {"metadata": o["metadata"]}})).collect();
    Ok(serde_json::json!({"columnDefinitions": [{"name": "Name", "priority": 0}], "rows": rows}))
}

fn cell_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "<none>".into(),
        other => other.to_string(),
    }
}

pub fn print_table(table: &Value, wide: bool, with_namespace: bool, kind_prefix: Option<&str>, headers: bool) {
    let defs = table["columnDefinitions"].as_array().cloned().unwrap_or_default();
    let keep: Vec<usize> =
        defs.iter().enumerate().filter(|(_, d)| wide || d["priority"].as_i64().unwrap_or(0) == 0).map(|(i, _)| i).collect();
    let mut rows: Vec<Vec<String>> = vec![];
    if headers {
        let mut h: Vec<String> = keep.iter().map(|i| defs[*i]["name"].as_str().unwrap_or("").to_uppercase()).collect();
        if with_namespace {
            h.insert(0, "NAMESPACE".into());
        }
        rows.push(h);
    }
    for r in table["rows"].as_array().into_iter().flatten() {
        let cells = r["cells"].as_array().cloned().unwrap_or_default();
        let mut row: Vec<String> = keep.iter().map(|i| cells.get(*i).map(cell_str).unwrap_or_default()).collect();
        if let (Some(p), Some(first)) = (kind_prefix, row.first_mut()) {
            *first = format!("{p}/{first}");
        }
        if with_namespace {
            row.insert(0, r.pointer("/object/metadata/namespace").and_then(Value::as_str).unwrap_or("").to_string());
        }
        rows.push(row);
    }
    print_rows(&rows);
}

/// Prints left-aligned columns separated by three spaces (like `tabwriter`).
pub fn print_rows(rows: &[Vec<String>]) {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> =
        (0..cols).map(|c| rows.iter().filter_map(|r| r.get(c)).map(|s| s.chars().count()).max().unwrap_or(0)).collect();
    for r in rows {
        let line: Vec<String> =
            r.iter().enumerate().map(|(i, s)| if i + 1 == r.len() { s.clone() } else { format!("{s:<w$}", w = widths[i]) }).collect();
        println!("{}", line.join("   ").trim_end());
    }
}

pub fn print_objects(objects: &[Value], fmt: &str, single: bool) -> Result<()> {
    let v = if single && objects.len() == 1 {
        objects[0].clone()
    } else {
        serde_json::json!({"apiVersion": "v1", "kind": "List", "metadata": {"resourceVersion": ""}, "items": objects})
    };
    if fmt == "json" {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        print!("{}", serde_yaml::to_string(&v)?);
    }
    Ok(())
}
