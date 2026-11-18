//! Container environment (`kubelet_pods.go: makeEnvironmentVariables`, `envvars`).
//!
//! Order: service links (`<SVC>_SERVICE_HOST/PORT`), `envFrom`, then `env`
//! (later entries win). `$(VAR)` references in `env` values and in
//! `command`/`args` are expanded against the variables defined so far;
//! `$$(VAR)` escapes.

use std::collections::BTreeMap;

use anyhow::{anyhow, Result};
use base64::Engine;
use k8s_api::core::v1::{ConfigMap, Container, Pod, Secret, Service};
use k8s_apimachinery::quantity::parse_milli;
use k8s_client_go::{Api, Client, ListParams};
use serde_json::Value;

use crate::volumes::field_ref;

/// `$(VAR)` expansion (`expansion.Expand`).
pub fn expand(s: &str, env: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' && i + 1 < b.len() {
            if b[i + 1] == b'$' {
                out.push('$');
                i += 2;
                continue;
            }
            if b[i + 1] == b'(' {
                if let Some(end) = s[i + 2..].find(')') {
                    let name = &s[i + 2..i + 2 + end];
                    match env.get(name) {
                        Some(v) => out.push_str(v),
                        None => out.push_str(&s[i..i + 3 + end]),
                    }
                    i += 3 + end;
                    continue;
                }
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

fn svc_env_name(name: &str) -> String {
    name.to_uppercase().replace('-', "_")
}

/// `<SVC>_SERVICE_HOST`, `<SVC>_SERVICE_PORT[_<NAME>]`, `<SVC>_PORT...` for one service.
fn service_vars(svc: &Service, out: &mut Vec<(String, String)>) {
    let Some(spec) = svc.spec.as_ref() else { return };
    let ip = spec.cluster_ip.clone().unwrap_or_default();
    if ip.is_empty() || ip == "None" {
        return;
    }
    let n = svc_env_name(svc.metadata.name());
    let ports = spec.ports.clone().unwrap_or_default();
    out.push((format!("{n}_SERVICE_HOST"), ip.clone()));
    if let Some(p) = ports.first() {
        out.push((format!("{n}_SERVICE_PORT"), p.port.to_string()));
        let proto = p.protocol.clone().unwrap_or_else(|| "TCP".into());
        out.push((format!("{n}_PORT"), format!("{}://{ip}:{}", proto.to_lowercase(), p.port)));
    }
    for p in &ports {
        let proto = p.protocol.clone().unwrap_or_else(|| "TCP".into());
        if let Some(pn) = &p.name {
            out.push((format!("{n}_SERVICE_PORT_{}", svc_env_name(pn)), p.port.to_string()));
        }
        let base = format!("{n}_PORT_{}_{}", p.port, proto.to_uppercase());
        out.push((base.clone(), format!("{}://{ip}:{}", proto.to_lowercase(), p.port)));
        out.push((format!("{base}_PROTO"), proto.to_lowercase()));
        out.push((format!("{base}_PORT"), p.port.to_string()));
        out.push((format!("{base}_ADDR"), ip.clone()));
    }
}

async fn cm(client: &Client, ns: &str, name: &str) -> Result<Option<ConfigMap>> {
    Ok(Api::<ConfigMap>::namespaced(client.clone(), ns).get_opt(name).await?)
}

async fn secret(client: &Client, ns: &str, name: &str) -> Result<Option<Secret>> {
    Ok(Api::<Secret>::namespaced(client.clone(), ns).get_opt(name).await?)
}

fn decode(v: &str) -> String {
    String::from_utf8_lossy(&base64::engine::general_purpose::STANDARD.decode(v).unwrap_or_default()).into_owned()
}

/// Builds the environment for one container.
pub async fn build(client: &Client, pod: &Pod, c: &Container) -> Result<Vec<(String, String)>> {
    let ns = pod.metadata.namespace();
    let mut ordered: Vec<(String, String)> = vec![];

    // Service links: the `kubernetes` service everywhere, plus services of the pod's namespace.
    let links = pod.spec.as_ref().and_then(|s| s.enable_service_links).unwrap_or(true);
    if let Some(k) = Api::<Service>::namespaced(client.clone(), "default").get_opt("kubernetes").await? {
        service_vars(&k, &mut ordered);
    }
    if links {
        let svcs = Api::<Service>::namespaced(client.clone(), ns).list(&ListParams::default()).await?;
        for s in svcs.items.iter().filter(|s| !(ns == "default" && s.metadata.name() == "kubernetes")) {
            service_vars(s, &mut ordered);
        }
    }

    for ef in c.env_from.iter().flatten() {
        let prefix = ef["prefix"].as_str().unwrap_or("");
        if let Some(r) = ef.get("configMapRef") {
            let name = r["name"].as_str().unwrap_or("");
            match cm(client, ns, name).await? {
                Some(m) => ordered.extend(m.data.unwrap_or_default().into_iter().map(|(k, v)| (format!("{prefix}{k}"), v))),
                None if r["optional"] == true => {}
                None => return Err(anyhow!("configmap \"{name}\" not found")),
            }
        }
        if let Some(r) = ef.get("secretRef") {
            let name = r["name"].as_str().unwrap_or("");
            match secret(client, ns, name).await? {
                Some(s) => ordered.extend(s.data.unwrap_or_default().into_iter().map(|(k, v)| (format!("{prefix}{k}"), decode(&v)))),
                None if r["optional"] == true => {}
                None => return Err(anyhow!("secret \"{name}\" not found")),
            }
        }
    }

    let mut map: BTreeMap<String, String> = ordered.iter().cloned().collect();
    for e in c.env.iter().flatten() {
        let value = match (&e.value, &e.value_from) {
            (Some(v), _) => Some(expand(v, &map)),
            (None, Some(from)) => resolve_from(client, pod, c, from).await?,
            (None, None) => Some(String::new()),
        };
        if let Some(v) = value {
            map.insert(e.name.clone(), v.clone());
            ordered.push((e.name.clone(), v));
        }
    }
    // Deduplicate keeping the last definition.
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<(String, String)> = ordered.into_iter().rev().filter(|(k, _)| seen.insert(k.clone())).collect();
    out.reverse();
    Ok(out)
}

async fn resolve_from(client: &Client, pod: &Pod, c: &Container, from: &Value) -> Result<Option<String>> {
    let ns = pod.metadata.namespace();
    if let Some(fp) = from.pointer("/fieldRef/fieldPath").and_then(Value::as_str) {
        return Ok(Some(field_ref(pod, fp).unwrap_or_default()));
    }
    if let Some(rf) = from.get("resourceFieldRef") {
        let res = rf["resource"].as_str().unwrap_or("");
        let target = rf["containerName"].as_str().map(|n| pod.spec.iter().flat_map(|s| s.containers.iter()).find(|x| x.name == n).cloned());
        let cont = target.flatten().unwrap_or_else(|| c.clone());
        let r = cont.resources.clone().unwrap_or_default();
        let (kind, name) = res.split_once('.').unwrap_or(("limits", res));
        let list = if kind == "requests" { r.requests } else { r.limits };
        let raw = list.and_then(|l| l.get(name).cloned());
        let divisor = rf["divisor"].as_str().map(|d| parse_milli(d).unwrap_or(1000)).unwrap_or(1000);
        return Ok(Some(match raw {
            Some(q) => {
                let m = parse_milli(&q).unwrap_or(0);
                ((m + divisor - 1) / divisor.max(1)).to_string()
            }
            // Unset limits report node allocatable in upstream; report 0 here.
            None => "0".into(),
        }));
    }
    for (kind, is_secret) in [("configMapKeyRef", false), ("secretKeyRef", true)] {
        if let Some(r) = from.get(kind) {
            let name = r["name"].as_str().unwrap_or("");
            let key = r["key"].as_str().unwrap_or("");
            let optional = r["optional"] == true;
            let val = if is_secret {
                secret(client, ns, name).await?.and_then(|s| s.data.unwrap_or_default().get(key).map(|v| decode(v)))
            } else {
                cm(client, ns, name).await?.and_then(|m| m.data.unwrap_or_default().get(key).cloned())
            };
            return match val {
                Some(v) => Ok(Some(v)),
                None if optional => Ok(None),
                None => Err(anyhow!("couldn't find key {key} in {} {ns}/{name}", if is_secret { "Secret" } else { "ConfigMap" })),
            };
        }
    }
    Ok(Some(String::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expansion() {
        let env: BTreeMap<String, String> = [("A".to_string(), "x".to_string())].into_iter().collect();
        assert_eq!(expand("$(A)-$(B)-$$(A)", &env), "x-$(B)-$(A)");
        assert_eq!(expand("plain", &env), "plain");
    }
}
