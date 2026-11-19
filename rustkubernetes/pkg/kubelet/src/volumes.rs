//! Pod volumes (`pkg/volume/{emptydir,hostpath,configmap,secret,downwardapi,projected}`
//! + `kubelet_volumes.go`).
//!
//! Volumes live under `<root-dir>/pods/<uid>/volumes/<plugin>/<name>` and are
//! bind-mounted into containers. ConfigMap/Secret/DownwardAPI content is
//! (re)written atomically on every sync, so updates reach running pods.

use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use k8s_api::core::v1::{ConfigMap, Pod, Secret};
use k8s_client_go::{Api, Client};
use serde_json::Value;

use crate::runtime::Mount;

/// `pkg/kubelet/token`: cache of TokenRequest results, refreshed once 80% of
/// the token lifetime (or 24h) has passed, like upstream's token manager.
struct CachedToken {
    token: String,
    fetched: std::time::Instant,
    ttl: std::time::Duration,
}

static TOKENS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, CachedToken>>> = std::sync::OnceLock::new();

async fn service_account_token(client: &Client, pod: &Pod, src: &Value) -> Result<String> {
    let ns = pod.metadata.namespace();
    let pod_name = pod.metadata.name.clone().unwrap_or_default();
    let uid = pod.metadata.uid.clone().unwrap_or_default();
    let sa = pod.spec.as_ref().and_then(|s| s.service_account_name.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| "default".into());
    let audience = src["audience"].as_str().unwrap_or("").to_string();
    let exp = src["expirationSeconds"].as_i64().unwrap_or(3600).max(600);
    let key = format!("{uid}/{sa}/{audience}/{exp}");
    let cache = TOKENS.get_or_init(Default::default);
    if let Some(t) = cache.lock().unwrap().get(&key) {
        let refresh_at = (t.ttl.mul_f64(0.8)).min(std::time::Duration::from_secs(24 * 3600));
        if t.fetched.elapsed() < refresh_at {
            return Ok(t.token.clone());
        }
    }
    let mut spec = serde_json::json!({
        "expirationSeconds": exp,
        "boundObjectRef": {"apiVersion": "v1", "kind": "Pod", "name": pod_name, "uid": uid},
    });
    if !audience.is_empty() {
        spec["audiences"] = serde_json::json!([audience]);
    }
    let body = serde_json::json!({"apiVersion": "authentication.k8s.io/v1", "kind": "TokenRequest", "spec": spec});
    let path = format!("/api/v1/namespaces/{ns}/serviceaccounts/{sa}/token");
    let resp = client
        .send_json(reqwest::Method::POST, &path, &body)
        .await
        .with_context(|| format!("requesting token for service account {ns}/{sa}"))?;
    let token = resp["status"]["token"].as_str().ok_or_else(|| anyhow!("TokenRequest returned no token"))?.to_string();
    let ttl = std::time::Duration::from_secs(resp["spec"]["expirationSeconds"].as_i64().unwrap_or(exp) as u64);
    cache.lock().unwrap().insert(key, CachedToken { token: token.clone(), fetched: std::time::Instant::now(), ttl });
    Ok(token)
}

/// Drops cached tokens of a removed pod.
pub fn forget_tokens(uid: &str) {
    if let Some(c) = TOKENS.get() {
        c.lock().unwrap().retain(|k, _| !k.starts_with(&format!("{uid}/")));
    }
}

pub fn pod_dir(root: &Path, uid: &str) -> PathBuf {
    root.join("pods").join(uid)
}

/// Writes `files` (relative path → content) into `dir`, replacing changed files
/// and removing stale ones. Each file is written to a temp file and renamed.
fn write_files(dir: &Path, files: &BTreeMap<String, (Vec<u8>, u32)>) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))?;
    for (rel, (data, mode)) in files {
        if rel.contains("..") || rel.starts_with('/') {
            bail!("invalid volume item path {rel:?}");
        }
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if std::fs::read(&path).ok().as_deref() != Some(data.as_slice()) {
            let tmp = path.with_extension("rk8s-tmp");
            std::fs::write(&tmp, data)?;
            std::fs::rename(&tmp, &path)?;
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(*mode))?;
    }
    // Remove files that are no longer part of the volume.
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if e.path().is_file() && !files.keys().any(|k| k == &name || k.starts_with(&format!("{name}/"))) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(())
}

fn mode_of(v: &Value, key: &str, default: u32) -> u32 {
    v.get(key).and_then(Value::as_u64).map(|m| m as u32).unwrap_or(default)
}

/// Key → file mapping honouring `items` and modes.
fn project(data: &BTreeMap<String, Vec<u8>>, src: &Value) -> Result<BTreeMap<String, (Vec<u8>, u32)>> {
    let default_mode = mode_of(src, "defaultMode", 0o644);
    let mut out = BTreeMap::new();
    match src.get("items").and_then(Value::as_array) {
        Some(items) => {
            for it in items {
                let key = it["key"].as_str().unwrap_or("");
                let path = it["path"].as_str().unwrap_or(key);
                let v = data.get(key).ok_or_else(|| anyhow!("configuration key {key:?} not found"))?;
                out.insert(path.to_string(), (v.clone(), mode_of(it, "mode", default_mode)));
            }
        }
        None => {
            for (k, v) in data {
                out.insert(k.clone(), (v.clone(), default_mode));
            }
        }
    }
    Ok(out)
}

async fn configmap_data(client: &Client, ns: &str, name: &str) -> Result<Option<BTreeMap<String, Vec<u8>>>> {
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), ns);
    Ok(api.get_opt(name).await?.map(|cm| {
        let mut m: BTreeMap<String, Vec<u8>> = cm.data.unwrap_or_default().into_iter().map(|(k, v)| (k, v.into_bytes())).collect();
        for (k, v) in cm.binary_data.unwrap_or_default() {
            m.insert(k, base64::engine::general_purpose::STANDARD.decode(v).unwrap_or_default());
        }
        m
    }))
}

async fn secret_data(client: &Client, ns: &str, name: &str) -> Result<Option<BTreeMap<String, Vec<u8>>>> {
    let api: Api<Secret> = Api::namespaced(client.clone(), ns);
    Ok(api.get_opt(name).await?.map(|s| {
        s.data
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k, base64::engine::general_purpose::STANDARD.decode(v).unwrap_or_default()))
            .collect()
    }))
}

/// Resolves a `fieldRef` path against the pod (`fieldpath.ExtractFieldPathAsString`).
pub fn field_ref(pod: &Pod, path: &str) -> Option<String> {
    let m = &pod.metadata;
    let fmt_map = |map: Option<&BTreeMap<String, String>>| {
        map.map(|m| m.iter().map(|(k, v)| format!("{k}=\"{v}\"")).collect::<Vec<_>>().join("\n")).unwrap_or_default()
    };
    if let Some(key) = path.strip_prefix("metadata.labels['").and_then(|r| r.strip_suffix("']")) {
        return m.labels.as_ref().and_then(|l| l.get(key).cloned());
    }
    if let Some(key) = path.strip_prefix("metadata.annotations['").and_then(|r| r.strip_suffix("']")) {
        return m.annotations.as_ref().and_then(|l| l.get(key).cloned());
    }
    let spec = pod.spec.as_ref();
    let status = pod.status.as_ref();
    Some(match path {
        "metadata.name" => m.name().to_string(),
        "metadata.namespace" => m.namespace().to_string(),
        "metadata.uid" => m.uid.clone().unwrap_or_default(),
        "metadata.labels" => fmt_map(m.labels.as_ref()),
        "metadata.annotations" => fmt_map(m.annotations.as_ref()),
        "spec.nodeName" => spec?.node_name.clone()?,
        "spec.serviceAccountName" => spec?.service_account_name.clone()?,
        "status.podIP" => status?.pod_ip.clone()?,
        "status.hostIP" => status?.host_ip.clone()?,
        _ => return None,
    })
}

fn downward_files(pod: &Pod, src: &Value) -> BTreeMap<String, (Vec<u8>, u32)> {
    let default_mode = mode_of(src, "defaultMode", 0o644);
    let mut out = BTreeMap::new();
    for it in src["items"].as_array().into_iter().flatten() {
        let path = it["path"].as_str().unwrap_or("");
        if let Some(fp) = it.pointer("/fieldRef/fieldPath").and_then(Value::as_str) {
            out.insert(path.to_string(), (field_ref(pod, fp).unwrap_or_default().into_bytes(), mode_of(it, "mode", default_mode)));
        }
    }
    out
}

/// Prepares every volume of the pod; returns volume name → host path.
/// A `hostPath` source (pod volume or PersistentVolume), with its type check.
async fn host_path(rt: &dyn crate::runtime::Runtime, hp: &Value) -> Result<PathBuf> {
    let p = PathBuf::from(hp["path"].as_str().unwrap_or(""));
    match hp["type"].as_str().unwrap_or("") {
        // Upstream's kubelet runs as root; ours may not, so the runtime creates what it can't.
        "DirectoryOrCreate" if !p.is_dir() => match std::fs::create_dir_all(&p) {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => rt.ensure_host_path(&p, true).await?,
            r => r.with_context(|| format!("creating hostPath {}", p.display()))?,
        },
        "FileOrCreate" if !p.exists() => {
            if let Some(parent) = p.parent().filter(|d| !d.exists()) {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::write(&p, b"") {
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => rt.ensure_host_path(&p, false).await?,
                r => r.with_context(|| format!("creating hostPath {}", p.display()))?,
            }
        }
        "Directory" if !p.is_dir() => bail!("hostPath type check failed: {} is not a directory", p.display()),
        "File" if !p.is_file() => bail!("hostPath type check failed: {} is not a file", p.display()),
        _ => {}
    }
    Ok(p)
}

/// `persistentVolumeClaim` and generic `ephemeral` volumes (volume manager
/// `desired_state_of_world_populator`): the claim must be bound; its volume's
/// `hostPath` or `local` path is mounted.
async fn persistent_volume_path(
    client: &Client,
    rt: &dyn crate::runtime::Runtime,
    pod: &Pod,
    vol: &str,
    src: &BTreeMap<String, Value>,
) -> Result<PathBuf> {
    use k8s_api::core::v1::{PersistentVolume, PersistentVolumeClaim};
    let ns = pod.metadata.namespace();
    let claim = match src.get("persistentVolumeClaim") {
        Some(c) => c["claimName"].as_str().unwrap_or("").to_string(),
        None => format!("{}-{vol}", pod.metadata.name()),
    };
    let pvc = k8s_client_go::Api::<PersistentVolumeClaim>::namespaced(client.clone(), ns)
        .get_opt(&claim)
        .await?
        .ok_or_else(|| anyhow::anyhow!("error processing PVC {ns}/{claim}: PVC {claim:?} not found"))?;
    if src.contains_key("ephemeral") {
        // The claim must belong to this pod (`ephemeral.VolumeIsForPod`).
        let owned = pvc.metadata.owner_references.iter().flatten().any(|o| Some(&o.uid) == pod.metadata.uid.as_ref());
        if !owned {
            bail!("error processing PVC {ns}/{claim}: PVC was not created for pod {ns}/{} (pod is not owner)", pod.metadata.name());
        }
    }
    let phase = pvc.status.as_ref().and_then(|s| s["phase"].as_str()).unwrap_or("");
    let vn = pvc.spec.as_ref().and_then(|s| s["volumeName"].as_str()).unwrap_or("");
    if phase != "Bound" || vn.is_empty() {
        bail!("error processing PVC {ns}/{claim}: PVC {ns}/{claim} has non-bound phase ({phase:?}) or empty pvc.Spec.VolumeName ({vn:?})");
    }
    let pv = k8s_client_go::Api::<PersistentVolume>::all(client.clone())
        .get_opt(vn)
        .await?
        .ok_or_else(|| anyhow::anyhow!("error processing PVC {ns}/{claim}: failed to fetch PV {vn:?} from API server: not found"))?;
    let spec = pv.spec.unwrap_or_default();
    if let Some(hp) = spec.get("hostPath").filter(|h| h.is_object()) {
        return host_path(rt, hp).await;
    }
    if let Some(l) = spec.get("local").filter(|l| l.is_object()) {
        let p = PathBuf::from(l["path"].as_str().unwrap_or(""));
        if !p.exists() {
            bail!("MountVolume.NewMounter initialization failed for volume {vn:?} : path {:?} does not exist", p.display().to_string());
        }
        return Ok(p);
    }
    bail!("volume {vol:?}: PersistentVolume {vn:?} has a volume source this kubelet cannot mount")
}

pub async fn setup(client: &Client, rt: &dyn crate::runtime::Runtime, root: &Path, pod: &Pod) -> Result<HashMap<String, PathBuf>> {
    let uid = pod.metadata.uid.clone().unwrap_or_default();
    let ns = pod.metadata.namespace();
    let base = pod_dir(root, &uid).join("volumes");
    let mut out = HashMap::new();
    for v in pod.spec.as_ref().and_then(|s| s.volumes.clone()).unwrap_or_default() {
        let src = &v.source;
        let path = if let Some(ed) = src.get("emptyDir") {
            let p = base.join("kubernetes.io~empty-dir").join(&v.name);
            std::fs::create_dir_all(&p)?;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o777))?;
            let _ = ed; // medium: Memory is served from disk (tmpfs needs root)
            p
        } else if let Some(hp) = src.get("hostPath") {
            host_path(rt, hp).await?
        } else if src.get("persistentVolumeClaim").is_some() || src.get("ephemeral").is_some_and(Value::is_object) {
            persistent_volume_path(client, rt, pod, &v.name, src).await?
        } else if let Some(cm) = src.get("configMap") {
            let p = base.join("kubernetes.io~configmap").join(&v.name);
            let name = cm["name"].as_str().unwrap_or("");
            match configmap_data(client, ns, name).await? {
                Some(d) => write_files(&p, &project(&d, cm)?)?,
                None if cm["optional"] == true => write_files(&p, &BTreeMap::new())?,
                None => bail!("configmap \"{name}\" not found"),
            }
            p
        } else if let Some(sec) = src.get("secret") {
            let p = base.join("kubernetes.io~secret").join(&v.name);
            let name = sec["secretName"].as_str().unwrap_or("");
            match secret_data(client, ns, name).await? {
                Some(d) => write_files(&p, &project(&d, sec)?)?,
                None if sec["optional"] == true => write_files(&p, &BTreeMap::new())?,
                None => bail!("secret \"{name}\" not found"),
            }
            p
        } else if let Some(da) = src.get("downwardAPI") {
            let p = base.join("kubernetes.io~downward-api").join(&v.name);
            write_files(&p, &downward_files(pod, da))?;
            p
        } else if let Some(pr) = src.get("projected") {
            let p = base.join("kubernetes.io~projected").join(&v.name);
            let mut files = BTreeMap::new();
            for s in pr["sources"].as_array().into_iter().flatten() {
                if let Some(cm) = s.get("configMap") {
                    if let Some(d) = configmap_data(client, ns, cm["name"].as_str().unwrap_or("")).await? {
                        files.extend(project(&d, cm)?);
                    }
                } else if let Some(sec) = s.get("secret") {
                    if let Some(d) = secret_data(client, ns, sec["name"].as_str().unwrap_or("")).await? {
                        files.extend(project(&d, sec)?);
                    }
                } else if let Some(da) = s.get("downwardAPI") {
                    files.extend(downward_files(pod, da));
                } else if let Some(t) = s.get("serviceAccountToken") {
                    let token = service_account_token(client, pod, t).await?;
                    let mode = pr["defaultMode"].as_u64().unwrap_or(0o644) as u32;
                    files.insert(t["path"].as_str().unwrap_or("token").to_string(), (token.into_bytes(), mode));
                }
            }
            write_files(&p, &files)?;
            p
        } else {
            let kind = src.keys().next().cloned().unwrap_or_default();
            bail!("volume {:?}: unsupported volume type {kind:?}", v.name);
        };
        out.insert(v.name.clone(), path);
    }
    Ok(out)
}

/// Bind mounts for a container's `volumeMounts` (honouring `subPath`).
pub fn mounts_for(container: &k8s_api::core::v1::Container, vols: &HashMap<String, PathBuf>) -> Result<Vec<Mount>> {
    let mut out = vec![];
    for m in container.volume_mounts.iter().flatten() {
        let base = vols.get(&m.name).ok_or_else(|| anyhow!("volume {:?} not found", m.name))?;
        let host = match m.sub_path.as_deref().filter(|s| !s.is_empty()) {
            Some(sp) => {
                if sp.contains("..") || sp.starts_with('/') {
                    bail!("invalid subPath {sp:?}");
                }
                let p = base.join(sp);
                if !p.exists() {
                    std::fs::create_dir_all(&p)?;
                }
                p
            }
            None => base.clone(),
        };
        out.push(Mount {
            host_path: host.to_string_lossy().into(),
            container_path: m.mount_path.clone(),
            read_only: m.read_only.unwrap_or(false),
        });
    }
    Ok(out)
}

/// Removes the pod's directory (`cleanupOrphanedPodDirs`).
pub fn teardown(root: &Path, uid: &str) {
    forget_tokens(uid);
    let dir = pod_dir(root, uid);
    if dir.starts_with(root) && dir.join("volumes").exists() {
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projects_items_and_writes_files() {
        let data: BTreeMap<String, Vec<u8>> = [("a".to_string(), b"1".to_vec()), ("b".to_string(), b"2".to_vec())].into_iter().collect();
        let all = project(&data, &json!({})).unwrap();
        assert_eq!(all.len(), 2);
        let some = project(&data, &json!({"items": [{"key": "a", "path": "sub/x.conf", "mode": 0o600}]})).unwrap();
        assert_eq!(some["sub/x.conf"], (b"1".to_vec(), 0o600));
        assert!(project(&data, &json!({"items": [{"key": "zz", "path": "p"}]})).is_err());

        let dir = std::env::temp_dir().join(format!("rk8s-vol-{}", std::process::id()));
        write_files(&dir, &all).unwrap();
        assert_eq!(std::fs::read(dir.join("a")).unwrap(), b"1");
        write_files(&dir, &project(&data, &json!({"items": [{"key": "b", "path": "b"}]})).unwrap()).unwrap();
        assert!(!dir.join("a").exists(), "stale key removed");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn field_refs() {
        let pod: Pod = serde_json::from_value(json!({"metadata": {"name": "p", "namespace": "n", "labels": {"app": "x"}},
            "spec": {"nodeName": "node1", "containers": []}, "status": {"podIP": "10.0.0.5"}}))
        .unwrap();
        assert_eq!(field_ref(&pod, "metadata.name").as_deref(), Some("p"));
        assert_eq!(field_ref(&pod, "metadata.labels['app']").as_deref(), Some("x"));
        assert_eq!(field_ref(&pod, "status.podIP").as_deref(), Some("10.0.0.5"));
        assert_eq!(field_ref(&pod, "spec.nodeName").as_deref(), Some("node1"));
    }
}
