//! Resource discovery (`client-go/discovery` + `restmapper`): maps `po`,
//! `pod`, `pods`, `Pod`, `deploy`, `deployments.apps` to an [`ApiResource`].

use anyhow::{anyhow, Result};
use k8s_apimachinery::unstructured::ObjectExt;
use k8s_client_go::api::ApiResource;
use k8s_client_go::Client;
use serde_json::Value;
use tokio::sync::OnceCell;

#[derive(Clone, Debug)]
pub struct Entry {
    pub resource: ApiResource,
    pub singular: String,
    pub short_names: Vec<String>,
    pub categories: Vec<String>,
}

pub struct Discovery {
    client: Client,
    cache: OnceCell<Vec<Entry>>,
}

impl Discovery {
    pub fn new(client: Client) -> Self {
        Self { client, cache: OnceCell::new() }
    }

    async fn load(&self) -> Result<Vec<Entry>> {
        let mut gvs = vec![("".to_string(), "v1".to_string())];
        let groups = self.client.get_json("/apis").await?;
        for g in groups["groups"].as_array().into_iter().flatten() {
            if let Some(v) = g.pointer("/preferredVersion/version").and_then(Value::as_str) {
                gvs.push((g["name"].as_str().unwrap_or("").to_string(), v.to_string()));
            }
        }
        let mut out = vec![];
        for (group, version) in gvs {
            let path = if group.is_empty() { format!("/api/{version}") } else { format!("/apis/{group}/{version}") };
            let list = self.client.get_json(&path).await?;
            for r in list["resources"].as_array().into_iter().flatten() {
                let name = r["name"].as_str().unwrap_or("");
                if name.contains('/') {
                    continue; // subresource
                }
                out.push(Entry {
                    resource: ApiResource {
                        group: group.clone(),
                        version: version.clone(),
                        plural: name.to_string(),
                        kind: r["kind"].as_str().unwrap_or("").to_string(),
                        namespaced: r["namespaced"].as_bool().unwrap_or(false),
                    },
                    singular: r["singularName"].as_str().unwrap_or("").to_string(),
                    short_names: strings(&r["shortNames"]),
                    categories: strings(&r["categories"]),
                });
            }
        }
        Ok(out)
    }

    pub async fn all(&self) -> Result<Vec<Entry>> {
        Ok(self.cache.get_or_try_init(|| self.load()).await?.clone())
    }

    /// Resolves a name that may be a category (`all`) into one or more resources.
    pub async fn resolve_many(&self, name: &str) -> Result<Vec<ApiResource>> {
        let in_category: Vec<ApiResource> =
            self.all().await?.into_iter().filter(|e| e.categories.iter().any(|c| c == name)).map(|e| e.resource).collect();
        if !in_category.is_empty() {
            return Ok(in_category);
        }
        Ok(vec![self.resolve(name).await?])
    }

    /// Resolves a user-typed resource name.
    pub async fn resolve(&self, name: &str) -> Result<ApiResource> {
        let lower = name.to_lowercase();
        let (res, group) = match lower.split_once('.') {
            Some((r, g)) => (r.to_string(), Some(g.to_string())),
            None => (lower.clone(), None),
        };
        let all = self.all().await?;
        all.iter()
            .filter(|e| group.as_ref().is_none_or(|g| &e.resource.group == g))
            .find(|e| {
                e.resource.plural == res || e.singular == res || e.resource.kind.to_lowercase() == res || e.short_names.contains(&res)
            })
            .map(|e| e.resource.clone())
            .ok_or_else(|| anyhow!("the server doesn't have a resource type \"{name}\""))
    }

    /// Resource for an object's `apiVersion` + `kind`.
    pub async fn for_object(&self, obj: &Value) -> Result<ApiResource> {
        let av = obj.api_version();
        let (group, version) = av.split_once('/').unwrap_or(("", av));
        let kind = obj.kind();
        self.all()
            .await?
            .into_iter()
            .find(|e| e.resource.group == group && e.resource.version == version && e.resource.kind == kind)
            .map(|e| e.resource)
            .ok_or_else(|| anyhow!("no matches for kind \"{kind}\" in version \"{av}\""))
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default()
}
