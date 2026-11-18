//! `ServiceAccount` admission plugin (`plugin/pkg/admission/serviceaccount`).
//!
//! For every new pod:
//! * `spec.serviceAccountName` defaults to `default`, and the ServiceAccount must exist;
//! * unless `automountServiceAccountToken: false` (on the pod or the ServiceAccount),
//!   a `kube-api-access-<suffix>` projected volume is added and mounted read-only at
//!   `/var/run/secrets/kubernetes.io/serviceaccount` in every container. It holds a
//!   bound, auto-rotated `token`, the cluster CA (`ca.crt`, from the
//!   `kube-root-ca.crt` ConfigMap) and the pod's `namespace`.

use async_trait::async_trait;
use k8s_apimachinery::errors::ApiError;
use k8s_apiserver::admission::{AdmissionPlugin, Attributes, Operation};
use k8s_apiserver::StrategyContext;
use serde_json::{json, Value};

pub const PLUGIN_NAME: &str = "ServiceAccount";
pub const MOUNT_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount";
pub const ROOT_CA_CONFIGMAP: &str = "kube-root-ca.crt";
/// `warnOnlyBoundTokenExpirationSeconds`: 1 hour + 7 s, like upstream.
pub const TOKEN_EXPIRATION_SECONDS: i64 = 3607;

pub struct ServiceAccount;

fn suffix() -> String {
    const ALPHABET: &[u8] = b"bcdfghjklmnpqrstvwxz2456789";
    let mut n =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos() as u64 ^ d.as_secs()).unwrap_or(7);
    (0..5)
        .map(|_| {
            n = n.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ALPHABET[((n >> 33) % ALPHABET.len() as u64) as usize] as char
        })
        .collect()
}

/// The volume upstream injects (`TokenVolumeSource`).
pub fn api_access_volume(name: &str) -> Value {
    json!({
        "name": name,
        "projected": {
            "defaultMode": 420,
            "sources": [
                {"serviceAccountToken": {"expirationSeconds": TOKEN_EXPIRATION_SECONDS, "path": "token"}},
                {"configMap": {"name": ROOT_CA_CONFIGMAP, "items": [{"key": "ca.crt", "path": "ca.crt"}]}},
                {"downwardAPI": {"items": [{"path": "namespace", "fieldRef": {"apiVersion": "v1", "fieldPath": "metadata.namespace"}}]}}
            ]
        }
    })
}

#[async_trait]
impl AdmissionPlugin for ServiceAccount {
    fn name(&self) -> &str {
        PLUGIN_NAME
    }

    async fn admit(&self, attrs: &Attributes<'_>, pod: &mut Value, ctx: &StrategyContext<'_>) -> Result<(), ApiError> {
        if attrs.operation != Operation::Create
            || !attrs.resource.group.is_empty()
            || attrs.resource.plural != "pods"
            || attrs.subresource.is_some()
        {
            return Ok(());
        }
        // Mirror pods (static pods) never get credentials.
        if pod.pointer("/metadata/annotations/kubernetes.io~1config.mirror").is_some() {
            return Ok(());
        }
        let spec = pod.get_mut("spec").ok_or_else(|| ApiError::bad_request("pod has no spec"))?;
        let sa_name = spec["serviceAccountName"].as_str().filter(|s| !s.is_empty()).unwrap_or("default").to_string();
        spec["serviceAccountName"] = json!(sa_name);
        let sa = ctx.storage.get(&format!("/registry/serviceaccounts/{}/{sa_name}", attrs.namespace)).await.map_err(|_| {
            ApiError::forbidden(format!(
                "pods \"{}\" is forbidden: error looking up service account {}/{sa_name}: serviceaccount \"{sa_name}\" not found",
                attrs.name, attrs.namespace
            ))
        })?;
        let automount =
            spec["automountServiceAccountToken"].as_bool().or_else(|| sa["automountServiceAccountToken"].as_bool()).unwrap_or(true);
        if !automount {
            return Ok(());
        }
        let containers = ["initContainers", "containers"];
        let already = containers.iter().any(|k| {
            spec[*k]
                .as_array()
                .into_iter()
                .flatten()
                .any(|c| c["volumeMounts"].as_array().into_iter().flatten().any(|m| m["mountPath"] == MOUNT_PATH))
        });
        if already {
            return Ok(());
        }
        let vol = format!("kube-api-access-{}", suffix());
        let vols = spec.as_object_mut().unwrap().entry("volumes").or_insert_with(|| json!([]));
        if let Some(v) = vols.as_array_mut() {
            v.push(api_access_volume(&vol));
        }
        for k in containers {
            for c in spec[k].as_array_mut().into_iter().flatten() {
                let mounts = c.as_object_mut().unwrap().entry("volumeMounts").or_insert_with(|| json!([]));
                if let Some(m) = mounts.as_array_mut() {
                    m.push(json!({"name": vol, "mountPath": MOUNT_PATH, "readOnly": true}));
                }
            }
        }
        Ok(())
    }
}
