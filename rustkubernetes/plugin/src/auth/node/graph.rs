//! Port of `plugin/pkg/auth/authorizer/node/graph.go` and `graph_populator.go`:
//! which objects each node may reach.
//!
//! Upstream keeps a directed graph `object → pod → node` fed by informers. Here
//! the same relations are kept per pod (and per PersistentVolume) from a storage
//! watch, and a path query walks them:
//!
//! | Object | Reachable from node N when |
//! |--------|---------------------------|
//! | pod | `spec.nodeName == N` |
//! | secret, configmap, PVC, ServiceAccount | referenced by a pod on N |
//! | PersistentVolume | bound (`spec.claimRef`) to a PVC referenced by a pod on N |
//! | secret | also: referenced by such a PersistentVolume |

use std::collections::{HashMap, HashSet};

use serde_json::Value;

/// `vertexType`
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Pod,
    Secret,
    ConfigMap,
    Pvc,
    Pv,
    ServiceAccount,
}

type Key = (String, String); // (namespace, name)

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PodRefs {
    pub node: String,
    pub service_account: String,
    pub secrets: HashSet<String>,
    pub config_maps: HashSet<String>,
    pub pvcs: HashSet<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PvRefs {
    /// The PVC the volume is bound to.
    pub claim: Option<Key>,
    /// Secrets the volume source references, with their namespaces.
    pub secrets: HashSet<Key>,
}

#[derive(Default)]
pub struct Graph {
    pods: HashMap<Key, PodRefs>,
    /// node → pods bound to it
    by_node: HashMap<String, HashSet<Key>>,
    pvs: HashMap<String, PvRefs>,
}

fn s<'a>(v: &'a Value, ptr: &str) -> &'a str {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or("")
}

fn items<'a>(v: &'a Value, ptr: &str) -> impl Iterator<Item = &'a Value> {
    v.pointer(ptr).and_then(Value::as_array).into_iter().flatten()
}

/// Volume sources that name a Secret in the pod's namespace (`VisitPodSecretNames`).
const POD_VOLUME_SECRET_REFS: &[&str] = &[
    "/azureFile/secretName",
    "/cephfs/secretRef/name",
    "/cinder/secretRef/name",
    "/flexVolume/secretRef/name",
    "/rbd/secretRef/name",
    "/secret/secretName",
    "/scaleIO/secretRef/name",
    "/iscsi/secretRef/name",
    "/storageos/secretRef/name",
    "/csi/nodePublishSecretRef/name",
];

/// What a pod references (`VisitPodSecretNames`, `VisitPodConfigmapNames`, PVC volumes).
pub fn pod_refs(pod: &Value) -> PodRefs {
    let mut r = PodRefs {
        node: s(pod, "/spec/nodeName").to_string(),
        service_account: s(pod, "/spec/serviceAccountName").to_string(),
        ..Default::default()
    };
    let add = |set: &mut HashSet<String>, n: &str| {
        if !n.is_empty() {
            set.insert(n.to_string());
        }
    };
    for ips in items(pod, "/spec/imagePullSecrets") {
        add(&mut r.secrets, s(ips, "/name"));
    }
    for kind in ["containers", "initContainers", "ephemeralContainers"] {
        for c in items(pod, &format!("/spec/{kind}")) {
            for e in items(c, "/env") {
                add(&mut r.secrets, s(e, "/valueFrom/secretKeyRef/name"));
                add(&mut r.config_maps, s(e, "/valueFrom/configMapKeyRef/name"));
            }
            for e in items(c, "/envFrom") {
                add(&mut r.secrets, s(e, "/secretRef/name"));
                add(&mut r.config_maps, s(e, "/configMapRef/name"));
            }
        }
    }
    let pod_name = s(pod, "/metadata/name");
    for v in items(pod, "/spec/volumes") {
        for p in POD_VOLUME_SECRET_REFS {
            add(&mut r.secrets, s(v, p));
        }
        add(&mut r.config_maps, s(v, "/configMap/name"));
        for src in items(v, "/projected/sources") {
            add(&mut r.secrets, s(src, "/secret/name"));
            add(&mut r.config_maps, s(src, "/configMap/name"));
        }
        add(&mut r.pvcs, s(v, "/persistentVolumeClaim/claimName"));
        // Generic ephemeral volumes create the PVC `<pod>-<volume>`.
        if v.get("ephemeral").is_some_and(Value::is_object) {
            add(&mut r.pvcs, &format!("{pod_name}-{}", s(v, "/name")));
        }
    }
    r
}

/// Secret references of a PersistentVolume source (`VisitPVSecretNames`).
pub fn pv_refs(pv: &Value) -> PvRefs {
    let mut r = PvRefs::default();
    if let Some(c) = pv.pointer("/spec/claimRef").filter(|c| c.is_object()) {
        if !s(c, "/name").is_empty() {
            r.claim = Some((s(c, "/namespace").to_string(), s(c, "/name").to_string()));
        }
    }
    let Some(spec) = pv.get("spec").and_then(Value::as_object) else { return r };
    let claim_ns = r.claim.as_ref().map(|c| c.0.clone()).unwrap_or_default();
    for (src, v) in spec {
        let Some(obj) = v.as_object() else { continue };
        for (field, refv) in obj {
            let is_ref = field == "secretRef" || field.ends_with("SecretRef");
            if is_ref && refv.is_object() {
                let name = s(refv, "/name");
                if !name.is_empty() {
                    let ns = refv.get("namespace").and_then(Value::as_str).filter(|n| !n.is_empty()).unwrap_or(&claim_ns);
                    r.secrets.insert((ns.to_string(), name.to_string()));
                }
            }
        }
        // azureFile names the secret directly, in secretNamespace (default "default").
        if src == "azureFile" {
            let name = s(v, "/secretName");
            if !name.is_empty() {
                let ns = v.get("secretNamespace").and_then(Value::as_str).filter(|n| !n.is_empty()).unwrap_or("default");
                r.secrets.insert((ns.to_string(), name.to_string()));
            }
        }
    }
    r
}

impl Graph {
    /// `AddPod` / `DeletePod`. Pods not bound to a node have no edges.
    pub fn set_pod(&mut self, ns: &str, name: &str, pod: Option<&Value>) {
        let key = (ns.to_string(), name.to_string());
        if let Some(old) = self.pods.remove(&key) {
            if let Some(set) = self.by_node.get_mut(&old.node) {
                set.remove(&key);
                if set.is_empty() {
                    self.by_node.remove(&old.node);
                }
            }
        }
        let Some(pod) = pod else { return };
        let refs = pod_refs(pod);
        if refs.node.is_empty() {
            return;
        }
        self.by_node.entry(refs.node.clone()).or_default().insert(key.clone());
        self.pods.insert(key, refs);
    }

    /// `AddPV` / `DeletePV`.
    pub fn set_pv(&mut self, name: &str, pv: Option<&Value>) {
        match pv {
            Some(v) => {
                self.pvs.insert(name.to_string(), pv_refs(v));
            }
            None => {
                self.pvs.remove(name);
            }
        }
    }

    pub fn clear_pods(&mut self) {
        self.pods.clear();
        self.by_node.clear();
    }

    pub fn clear_pvs(&mut self) {
        self.pvs.clear();
    }

    fn node_pods(&self, node: &str) -> impl Iterator<Item = (&Key, &PodRefs)> {
        self.by_node.get(node).into_iter().flatten().filter_map(|k| self.pods.get_key_value(k))
    }

    fn pvc_on_node(&self, node: &str, ns: &str, name: &str) -> bool {
        self.node_pods(node).any(|((pns, _), r)| pns == ns && r.pvcs.contains(name))
    }

    /// `hasPathFrom`: is there a path from the object to the node?
    pub fn has_path(&self, node: &str, kind: Kind, ns: &str, name: &str) -> bool {
        match kind {
            Kind::Pod => self.pods.get(&(ns.to_string(), name.to_string())).is_some_and(|r| r.node == node),
            Kind::Secret => {
                self.node_pods(node).any(|((pns, _), r)| pns == ns && r.secrets.contains(name))
                    || self.pvs.values().any(|pv| {
                        pv.secrets.contains(&(ns.to_string(), name.to_string()))
                            && pv.claim.as_ref().is_some_and(|(cns, cn)| self.pvc_on_node(node, cns, cn))
                    })
            }
            Kind::ConfigMap => self.node_pods(node).any(|((pns, _), r)| pns == ns && r.config_maps.contains(name)),
            Kind::Pvc => self.pvc_on_node(node, ns, name),
            Kind::Pv => self.pvs.get(name).and_then(|pv| pv.claim.as_ref()).is_some_and(|(cns, cn)| self.pvc_on_node(node, cns, cn)),
            Kind::ServiceAccount => self.node_pods(node).any(|((pns, _), r)| pns == ns && r.service_account == name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pod() -> Value {
        json!({"metadata": {"namespace": "ns", "name": "p"}, "spec": {
        "nodeName": "n1", "serviceAccountName": "sa",
        "imagePullSecrets": [{"name": "pull"}],
        "initContainers": [{"name": "i", "envFrom": [{"configMapRef": {"name": "cm-envfrom"}}]}],
        "containers": [{"name": "c", "env": [{"name": "X", "valueFrom": {"secretKeyRef": {"name": "env-secret", "key": "k"}}}]}],
        "volumes": [
            {"name": "a", "secret": {"secretName": "vol-secret"}},
            {"name": "b", "projected": {"sources": [{"configMap": {"name": "kube-root-ca.crt"}}, {"serviceAccountToken": {"path": "token"}}]}},
            {"name": "c", "persistentVolumeClaim": {"claimName": "data"}},
            {"name": "eph", "ephemeral": {"volumeClaimTemplate": {}}},
            {"name": "d", "csi": {"driver": "x", "nodePublishSecretRef": {"name": "csi-secret"}}}
        ]}})
    }

    #[test]
    fn pod_references() {
        let r = pod_refs(&pod());
        for sname in ["pull", "env-secret", "vol-secret", "csi-secret"] {
            assert!(r.secrets.contains(sname), "{sname}");
        }
        assert!(r.config_maps.contains("cm-envfrom") && r.config_maps.contains("kube-root-ca.crt"));
        assert!(r.pvcs.contains("data") && r.pvcs.contains("p-eph"));
    }

    #[test]
    fn paths() {
        let mut g = Graph::default();
        g.set_pod("ns", "p", Some(&pod()));
        g.set_pv("pv1", Some(&json!({"spec": {"claimRef": {"namespace": "ns", "name": "data"}, "csi": {"driver": "x", "nodeStageSecretRef": {"name": "stage", "namespace": "kube-system"}}}})));
        assert!(g.has_path("n1", Kind::Pod, "ns", "p"));
        assert!(!g.has_path("n2", Kind::Pod, "ns", "p"));
        assert!(g.has_path("n1", Kind::Secret, "ns", "vol-secret"));
        assert!(!g.has_path("n1", Kind::Secret, "other", "vol-secret"));
        assert!(!g.has_path("n2", Kind::Secret, "ns", "vol-secret"));
        assert!(g.has_path("n1", Kind::ConfigMap, "ns", "kube-root-ca.crt"));
        assert!(g.has_path("n1", Kind::ServiceAccount, "ns", "sa"));
        assert!(g.has_path("n1", Kind::Pvc, "ns", "data"));
        assert!(g.has_path("n1", Kind::Pv, "", "pv1"));
        assert!(g.has_path("n1", Kind::Secret, "kube-system", "stage"));
        // Rescheduling the pod moves its edges; deleting removes them.
        let mut moved = pod();
        moved["spec"]["nodeName"] = json!("n2");
        g.set_pod("ns", "p", Some(&moved));
        assert!(!g.has_path("n1", Kind::Secret, "ns", "vol-secret") && g.has_path("n2", Kind::Secret, "ns", "vol-secret"));
        g.set_pod("ns", "p", None);
        assert!(!g.has_path("n2", Kind::Secret, "ns", "vol-secret"));
        let mut unbound = pod();
        unbound["spec"]["nodeName"] = json!("");
        g.set_pod("ns", "p", Some(&unbound));
        assert!(!g.has_path("", Kind::Pod, "ns", "p"));
    }
}
