//! Port of `client-go/discovery`: which resources does the server serve?

use serde_json::Value;

use crate::api::ApiResource;
use crate::rest::{Client, Error};

#[derive(Clone, Debug)]
pub struct ServerResource {
    pub resource: ApiResource,
    pub verbs: Vec<String>,
    pub short_names: Vec<String>,
}

impl ServerResource {
    pub fn supports(&self, verb: &str) -> bool {
        self.verbs.iter().any(|v| v == verb)
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// `ServerPreferredResources`: every top-level resource (no subresources) in each
/// group's preferred version.
pub async fn server_preferred_resources(client: &Client) -> Result<Vec<ServerResource>, Error> {
    let mut gvs = vec![(String::new(), "v1".to_string())];
    let groups = client.get_json("/apis").await?;
    for g in groups["groups"].as_array().into_iter().flatten() {
        if let Some(v) = g.pointer("/preferredVersion/version").and_then(Value::as_str) {
            gvs.push((g["name"].as_str().unwrap_or("").to_string(), v.to_string()));
        }
    }
    let mut out = vec![];
    for (group, version) in gvs {
        let path = if group.is_empty() { format!("/api/{version}") } else { format!("/apis/{group}/{version}") };
        let list = client.get_json(&path).await?;
        for r in list["resources"].as_array().into_iter().flatten() {
            let name = r["name"].as_str().unwrap_or("");
            if name.contains('/') {
                continue;
            }
            out.push(ServerResource {
                resource: ApiResource {
                    group: group.clone(),
                    version: version.clone(),
                    plural: name.to_string(),
                    kind: r["kind"].as_str().unwrap_or("").to_string(),
                    namespaced: r["namespaced"].as_bool().unwrap_or(false),
                },
                verbs: strings(&r["verbs"]),
                short_names: strings(&r["shortNames"]),
            });
        }
    }
    Ok(out)
}
