//! `storage/volume`: what binds a PersistentVolume to a PersistentVolumeClaim.

pub mod volume {
    use std::collections::{BTreeMap, BTreeSet, HashSet};

    use k8s_api::core::v1::{PersistentVolume, PersistentVolumeClaim};
    use k8s_apimachinery::quantity::parse_value;
    use serde_json::Value;

    use crate::scheduling::nodeaffinity::{node_selector_matches, requirement_matches};

    /// `AnnBindCompleted`: set on a claim once the binding is complete.
    pub const ANN_BIND_COMPLETED: &str = "pv.kubernetes.io/bind-completed";
    /// `AnnBoundByController`: the binding was made by the controller, not the user.
    pub const ANN_BOUND_BY_CONTROLLER: &str = "pv.kubernetes.io/bound-by-controller";
    /// `AnnSelectedNode`: set by the scheduler for `WaitForFirstConsumer` provisioning.
    pub const ANN_SELECTED_NODE: &str = "volume.kubernetes.io/selected-node";
    /// `AnnDynamicallyProvisioned`: the plugin/provisioner that created a volume.
    pub const ANN_DYNAMICALLY_PROVISIONED: &str = "pv.kubernetes.io/provisioned-by";
    pub const ANN_STORAGE_PROVISIONER: &str = "volume.kubernetes.io/storage-provisioner";
    pub const ANN_BETA_STORAGE_PROVISIONER: &str = "volume.beta.kubernetes.io/storage-provisioner";
    /// `BetaStorageClassAnnotation`
    pub const ANN_BETA_STORAGE_CLASS: &str = "volume.beta.kubernetes.io/storage-class";
    /// `NotSupportedProvisioner`: static (pre-created) volumes only, e.g. `local`.
    pub const NOT_SUPPORTED_PROVISIONER: &str = "kubernetes.io/no-provisioner";
    pub const PV_PROTECTION_FINALIZER: &str = "kubernetes.io/pv-protection";
    pub const PVC_PROTECTION_FINALIZER: &str = "kubernetes.io/pvc-protection";

    fn s<'a>(v: Option<&'a Value>, p: &str) -> &'a str {
        v.and_then(|v| v.pointer(p)).and_then(Value::as_str).unwrap_or("")
    }

    fn ann<'a>(m: &'a k8s_apimachinery::meta::ObjectMeta, k: &str) -> Option<&'a str> {
        m.annotations.as_ref().and_then(|a| a.get(k)).map(String::as_str)
    }

    /// `GetPersistentVolumeClaimClass`: the beta annotation wins over `storageClassName`.
    pub fn claim_class(c: &PersistentVolumeClaim) -> String {
        if let Some(a) = ann(&c.metadata, ANN_BETA_STORAGE_CLASS) {
            return a.to_string();
        }
        s(c.spec.as_ref(), "/storageClassName").to_string()
    }

    /// Whether the claim has no class at all (`storageClassName` unset, no annotation).
    pub fn claim_class_unset(c: &PersistentVolumeClaim) -> bool {
        ann(&c.metadata, ANN_BETA_STORAGE_CLASS).is_none()
            && c.spec.as_ref().and_then(|s| s.get("storageClassName")).is_none_or(Value::is_null)
    }

    /// `GetPersistentVolumeClass`
    pub fn volume_class(v: &PersistentVolume) -> String {
        if let Some(a) = ann(&v.metadata, ANN_BETA_STORAGE_CLASS) {
            return a.to_string();
        }
        s(v.spec.as_ref(), "/storageClassName").to_string()
    }

    pub fn claim_volume_name(c: &PersistentVolumeClaim) -> &str {
        s(c.spec.as_ref(), "/volumeName")
    }

    pub fn volume_phase(v: &PersistentVolume) -> &str {
        s(v.status.as_ref(), "/phase")
    }

    pub fn claim_phase(c: &PersistentVolumeClaim) -> &str {
        s(c.status.as_ref(), "/phase")
    }

    pub fn claim_ref(v: &PersistentVolume) -> Option<&Value> {
        v.spec.as_ref().and_then(|s| s.get("claimRef")).filter(|r| r.is_object())
    }

    /// `IsVolumeBoundToClaim`: claimRef names the claim (and its UID, if set).
    pub fn is_volume_bound_to_claim(v: &PersistentVolume, c: &PersistentVolumeClaim) -> bool {
        let Some(r) = claim_ref(v) else { return false };
        if r["name"].as_str() != c.metadata.name.as_deref() || r["namespace"].as_str() != c.metadata.namespace.as_deref() {
            return false;
        }
        let uid = r["uid"].as_str().unwrap_or("");
        uid.is_empty() || Some(uid) == c.metadata.uid.as_deref()
    }

    /// Requested storage of a claim, in bytes.
    pub fn claim_request(c: &PersistentVolumeClaim) -> i128 {
        parse_value(s(c.spec.as_ref(), "/resources/requests/storage")).unwrap_or(0)
    }

    /// Capacity of a volume, in bytes.
    pub fn volume_capacity(v: &PersistentVolume) -> i128 {
        parse_value(s(v.spec.as_ref(), "/capacity/storage")).unwrap_or(0)
    }

    fn modes(v: Option<&Value>) -> Vec<String> {
        v.and_then(|v| v.get("accessModes"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|m| m.as_str().map(String::from))
            .collect()
    }

    pub fn claim_access_modes(c: &PersistentVolumeClaim) -> Vec<String> {
        modes(c.spec.as_ref())
    }

    pub fn volume_access_modes(v: &PersistentVolume) -> Vec<String> {
        modes(v.spec.as_ref())
    }

    /// `CheckAccessModes`: the volume offers every mode the claim asks for.
    pub fn check_access_modes(c: &PersistentVolumeClaim, v: &PersistentVolume) -> bool {
        let pv = volume_access_modes(v);
        claim_access_modes(c).iter().all(|m| pv.contains(m))
    }

    /// `CheckVolumeModeMismatches` (`Filesystem` when unset).
    pub fn volume_mode_mismatch(c: &PersistentVolumeClaim, v: &PersistentVolume) -> bool {
        let mode = |x: Option<&Value>| x.and_then(|x| x.get("volumeMode")).and_then(Value::as_str).unwrap_or("Filesystem").to_string();
        mode(c.spec.as_ref()) != mode(v.spec.as_ref())
    }

    /// `CheckNodeAffinity`: a volume without node affinity fits every node.
    pub fn check_node_affinity(v: &PersistentVolume, labels: &BTreeMap<String, String>, node_name: &str) -> Result<(), String> {
        let Some(req) = v.spec.as_ref().and_then(|s| s.pointer("/nodeAffinity/required")).filter(|r| r.is_object()) else {
            return Ok(());
        };
        if node_selector_matches(req, labels, node_name) {
            Ok(())
        } else {
            Err("no matching NodeSelectorTerms".into())
        }
    }

    /// `LabelSelector` (JSON) against labels; `None` matches everything.
    pub fn selector_matches(sel: Option<&Value>, labels: &BTreeMap<String, String>) -> bool {
        let Some(sel) = sel.filter(|s| s.is_object()) else { return true };
        sel["matchLabels"].as_object().is_none_or(|m| m.iter().all(|(k, v)| labels.get(k).map(String::as_str) == v.as_str()))
            && sel["matchExpressions"].as_array().is_none_or(|a| a.iter().all(|r| requirement_matches(r, labels)))
    }

    /// A node to check volume node affinity against.
    pub struct NodeRef<'a> {
        pub name: &'a str,
        pub labels: &'a BTreeMap<String, String>,
    }

    /// `FindMatchingVolume`: the smallest volume that satisfies the claim, or the
    /// volume already (pre-)bound to it. With `delay_binding` and no node, only
    /// pre-bound volumes are returned.
    pub fn find_matching_volume<'a>(
        c: &PersistentVolumeClaim,
        volumes: &[&'a PersistentVolume],
        node: Option<&NodeRef>,
        excluded: &HashSet<String>,
        delay_binding: bool,
    ) -> Option<&'a PersistentVolume> {
        let requested = claim_request(c);
        let class = claim_class(c);
        let selector = c.spec.as_ref().and_then(|s| s.get("selector"));
        let mut best: Option<&PersistentVolume> = None;
        for v in volumes.iter().copied() {
            if excluded.contains(v.metadata.name.as_deref().unwrap_or("")) {
                continue;
            }
            if claim_ref(v).is_some() && !is_volume_bound_to_claim(v, c) {
                continue;
            }
            let qty = volume_capacity(v);
            if qty < requested || volume_mode_mismatch(c, v) || v.metadata.deletion_timestamp.is_some() {
                continue;
            }
            let affinity_ok = node.is_none_or(|n| check_node_affinity(v, n.labels, n.name).is_ok());
            if is_volume_bound_to_claim(v, c) {
                return affinity_ok.then_some(v);
            }
            if node.is_none() && delay_binding {
                continue;
            }
            if volume_phase(v) != "Available" {
                continue;
            }
            let labels = v.metadata.labels.clone().unwrap_or_default();
            if !selector_matches(selector, &labels) || volume_class(v) != class || !affinity_ok {
                continue;
            }
            if node.is_some() && !check_access_modes(c, v) {
                continue;
            }
            if best.is_none_or(|b| volume_capacity(b) > qty) {
                best = Some(v);
            }
        }
        best
    }

    /// `persistentVolumeOrderedIndex.findByClaim`: volumes grouped by access-mode set;
    /// sets that contain all requested modes are tried smallest first.
    pub fn find_best_match_for_claim<'a>(
        c: &PersistentVolumeClaim,
        volumes: &[&'a PersistentVolume],
        delay_binding: bool,
    ) -> Option<&'a PersistentVolume> {
        let wanted: BTreeSet<String> = claim_access_modes(c).into_iter().collect();
        let mut groups: BTreeMap<BTreeSet<String>, Vec<&PersistentVolume>> = BTreeMap::new();
        for v in volumes.iter().copied() {
            groups.entry(volume_access_modes(v).into_iter().collect()).or_default().push(v);
        }
        let mut candidates: Vec<(&BTreeSet<String>, &Vec<&PersistentVolume>)> =
            groups.iter().filter(|(m, _)| m.is_superset(&wanted)).collect();
        candidates.sort_by_key(|(m, _)| m.len());
        candidates.into_iter().find_map(|(_, vs)| find_matching_volume(c, vs, None, &HashSet::new(), delay_binding))
    }

    /// `GetBindVolumeToClaim`: the volume with `claimRef` pointing at the claim.
    pub fn bind_volume_to_claim(v: &PersistentVolume, c: &PersistentVolumeClaim) -> (PersistentVolume, bool) {
        let mut out = v.clone();
        let mut dirty = false;
        let set_by_controller = !is_volume_bound_to_claim(v, c);
        let r = claim_ref(v);
        if r.is_none_or(|r| {
            r["name"].as_str() != c.metadata.name.as_deref()
                || r["namespace"].as_str() != c.metadata.namespace.as_deref()
                || r["uid"].as_str() != c.metadata.uid.as_deref()
        }) {
            let spec = out.spec.get_or_insert_with(|| serde_json::json!({}));
            spec["claimRef"] = serde_json::json!({
                "kind": "PersistentVolumeClaim", "namespace": c.metadata.namespace, "name": c.metadata.name,
                "uid": c.metadata.uid, "apiVersion": "v1", "resourceVersion": c.metadata.resource_version,
            });
            dirty = true;
        }
        if set_by_controller && ann(&out.metadata, ANN_BOUND_BY_CONTROLLER).is_none() {
            out.metadata.annotations.get_or_insert_with(BTreeMap::new).insert(ANN_BOUND_BY_CONTROLLER.into(), "yes".into());
            dirty = true;
        }
        (out, dirty)
    }

    /// The claims a pod uses: `persistentVolumeClaim` volumes and generic ephemeral
    /// volumes (`<pod>-<volume>`).
    pub fn pod_claim_names(pod: &k8s_api::core::v1::Pod) -> Vec<String> {
        let name = pod.metadata.name.as_deref().unwrap_or("");
        pod.spec
            .as_ref()
            .and_then(|s| s.volumes.as_ref())
            .into_iter()
            .flatten()
            .filter_map(|v| {
                if let Some(c) = v.source.get("persistentVolumeClaim") {
                    c["claimName"].as_str().map(String::from)
                } else if v.source.get("ephemeral").is_some_and(Value::is_object) {
                    Some(format!("{name}-{}", v.name))
                } else {
                    None
                }
            })
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;

        fn pv(name: &str, cap: &str, modes: &[&str], class: &str) -> PersistentVolume {
            serde_json::from_value(json!({"metadata": {"name": name}, "spec": {"capacity": {"storage": cap}, "accessModes": modes,
                "storageClassName": class, "hostPath": {"path": "/x"}}, "status": {"phase": "Available"}}))
            .unwrap()
        }

        fn pvc(req: &str, modes: &[&str], class: &str) -> PersistentVolumeClaim {
            serde_json::from_value(json!({"metadata": {"name": "c", "namespace": "ns", "uid": "u1"},
                "spec": {"accessModes": modes, "resources": {"requests": {"storage": req}}, "storageClassName": class}}))
            .unwrap()
        }

        #[test]
        fn smallest_matching_volume() {
            let (a, b, c) = (
                pv("a", "10Gi", &["ReadWriteOnce"], "x"),
                pv("b", "2Gi", &["ReadWriteOnce"], "x"),
                pv("c", "1Gi", &["ReadWriteOnce"], "x"),
            );
            let vs = vec![&a, &b, &c];
            let claim = pvc("1500Mi", &["ReadWriteOnce"], "x");
            assert_eq!(find_best_match_for_claim(&claim, &vs, false).unwrap().metadata.name.as_deref(), Some("b"));
            // Delayed binding without a node: nothing (only pre-bound volumes).
            assert!(find_best_match_for_claim(&claim, &vs, true).is_none());
            // Wrong class or modes.
            assert!(find_best_match_for_claim(&pvc("1Gi", &["ReadWriteOnce"], "y"), &vs, false).is_none());
            assert!(find_best_match_for_claim(&pvc("1Gi", &["ReadWriteMany"], "x"), &vs, false).is_none());
        }

        #[test]
        fn access_mode_groups_prefer_fewer_modes() {
            let rwx = pv("rwx", "1Gi", &["ReadWriteOnce", "ReadWriteMany"], "x");
            let rwo = pv("rwo", "5Gi", &["ReadWriteOnce"], "x");
            let vs = vec![&rwx, &rwo];
            assert_eq!(
                find_best_match_for_claim(&pvc("1Gi", &["ReadWriteOnce"], "x"), &vs, false).unwrap().metadata.name.as_deref(),
                Some("rwo")
            );
        }

        #[test]
        fn binding_and_affinity() {
            let mut v = pv("local", "1Gi", &["ReadWriteOnce"], "local");
            v.spec.as_mut().unwrap()["nodeAffinity"] = json!({"required": {"nodeSelectorTerms": [{"matchExpressions": [
                {"key": "kubernetes.io/hostname", "operator": "In", "values": ["n1"]}]}]}});
            let n1: BTreeMap<String, String> = [("kubernetes.io/hostname".to_string(), "n1".to_string())].into();
            let n2: BTreeMap<String, String> = [("kubernetes.io/hostname".to_string(), "n2".to_string())].into();
            assert!(check_node_affinity(&v, &n1, "n1").is_ok() && check_node_affinity(&v, &n2, "n2").is_err());
            let claim = pvc("1Gi", &["ReadWriteOnce"], "local");
            let on_n2 = NodeRef { name: "n2", labels: &n2 };
            assert!(find_matching_volume(&claim, &[&v], Some(&on_n2), &HashSet::new(), true).is_none());
            let (bound, dirty) = bind_volume_to_claim(&v, &claim);
            assert!(dirty && is_volume_bound_to_claim(&bound, &claim));
            assert_eq!(bound.metadata.annotations.unwrap()[ANN_BOUND_BY_CONTROLLER], "yes");
        }
    }
}
