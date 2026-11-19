//! Port of `pkg/registry/core/{persistentvolume,persistentvolumeclaim}` and
//! `pkg/registry/storage/storageclass`: defaults, validation and table printers.

use k8s_apimachinery::validation::{ErrorList, FieldError};
use k8s_apiserver::table::{age_cell, Column};
use serde_json::{json, Value};

use crate::generic::{str_at, SimpleStrategy};

const ACCESS_MODES: &[&str] = &["ReadOnlyMany", "ReadWriteMany", "ReadWriteOnce", "ReadWriteOncePod"];
/// Volume sources a PersistentVolume may use (`PersistentVolumeSource`).
const PV_SOURCES: &[&str] = &[
    "hostPath",
    "local",
    "nfs",
    "iscsi",
    "csi",
    "cephfs",
    "rbd",
    "fc",
    "flexVolume",
    "cinder",
    "azureFile",
    "azureDisk",
    "gcePersistentDisk",
    "awsElasticBlockStore",
    "glusterfs",
    "vsphereVolume",
    "portworxVolume",
    "scaleIO",
    "storageos",
    "photonPersistentDisk",
    "quobyte",
    "flocker",
];

/// `GetAccessModesAsString`: `RWO,ROX,RWX,RWOP`, in that order.
pub fn access_modes_string(modes: Option<&Value>) -> String {
    let have: Vec<&str> = modes.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    [("ReadWriteOnce", "RWO"), ("ReadOnlyMany", "ROX"), ("ReadWriteMany", "RWX"), ("ReadWriteOncePod", "RWOP")]
        .iter()
        .filter(|(m, _)| have.contains(m))
        .map(|(_, a)| *a)
        .collect::<Vec<_>>()
        .join(",")
}

fn class_of(obj: &Value) -> String {
    match obj.pointer("/metadata/annotations/volume.beta.kubernetes.io~1storage-class").and_then(Value::as_str) {
        Some(c) => c.to_string(),
        None => str_at(obj, "/spec/storageClassName").to_string(),
    }
}

fn validate_modes(obj: &Value, path: &str, errs: &mut ErrorList) {
    let modes: Vec<&str> =
        obj.pointer("/spec/accessModes").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    if modes.is_empty() {
        errs.push(FieldError::required(path, "at least 1 access mode is required"));
    }
    for m in &modes {
        if !ACCESS_MODES.contains(m) {
            errs.push(FieldError::not_supported(path, m, ACCESS_MODES));
        }
    }
    if modes.contains(&"ReadWriteOncePod") && modes.len() > 1 {
        errs.push(FieldError::forbidden(path, "may not use ReadWriteOncePod with other access modes"));
    }
}

fn valid_quantity(q: &str) -> bool {
    k8s_apimachinery::quantity::parse_value(q).is_ok_and(|v| v >= 0)
}

// ---------------------------------------------------------------------------
// PersistentVolume
// ---------------------------------------------------------------------------

pub fn persistent_volume() -> SimpleStrategy {
    SimpleStrategy::new(true).defaults(pv_defaults).validate(pv_validate).validate_update(pv_validate_update).table(pv_columns, pv_cells)
}

/// `SetDefaults_PersistentVolume` + `PrepareForCreate` (phase `Pending`).
fn pv_defaults(obj: &mut Value) {
    if let Some(spec) = obj.get_mut("spec").and_then(Value::as_object_mut) {
        spec.entry("persistentVolumeReclaimPolicy").or_insert(json!("Retain"));
        spec.entry("volumeMode").or_insert(json!("Filesystem"));
    }
    if obj.pointer("/status/phase").is_none() {
        obj["status"] = json!({"phase": "Pending"});
    }
}

fn pv_validate(obj: &Value) -> ErrorList {
    let mut errs = vec![];
    match obj.pointer("/spec/capacity/storage").and_then(Value::as_str) {
        None => errs.push(FieldError::required("spec.capacity", "")),
        Some(q) if !valid_quantity(q) => errs.push(FieldError::invalid("spec.capacity[storage]", q, "must be a valid quantity")),
        _ => {}
    }
    validate_modes(obj, "spec.accessModes", &mut errs);
    let sources: Vec<&str> =
        PV_SOURCES.iter().copied().filter(|k| obj.pointer(&format!("/spec/{k}")).is_some_and(Value::is_object)).collect();
    match sources.len() {
        0 => errs.push(FieldError::required("spec", "must specify a volume type")),
        1 => {}
        _ => errs.push(FieldError::forbidden("spec", "may not specify more than 1 volume type")),
    }
    if sources.contains(&"hostPath") && str_at(obj, "/spec/hostPath/path").is_empty() {
        errs.push(FieldError::required("spec.hostPath.path", ""));
    }
    if sources.contains(&"local") {
        if str_at(obj, "/spec/local/path").is_empty() {
            errs.push(FieldError::required("spec.local.path", ""));
        }
        if obj.pointer("/spec/nodeAffinity").is_none_or(Value::is_null) {
            errs.push(FieldError::required("spec.nodeAffinity", "Local volume requires node affinity"));
        }
    }
    let rp = str_at(obj, "/spec/persistentVolumeReclaimPolicy");
    if !rp.is_empty() && !["Delete", "Recycle", "Retain"].contains(&rp) {
        errs.push(FieldError::not_supported("spec.persistentVolumeReclaimPolicy", rp, &["Delete", "Recycle", "Retain"]));
    }
    errs
}

/// `ValidatePersistentVolumeUpdate`: the volume source cannot change.
fn pv_validate_update(new: &Value, old: &Value) -> ErrorList {
    let mut errs = pv_validate(new);
    let src = |v: &Value| PV_SOURCES.iter().map(|k| v.pointer(&format!("/spec/{k}")).cloned()).collect::<Vec<_>>();
    if src(new) != src(old) {
        errs.push(FieldError::forbidden("spec.persistentvolumesource", "spec.persistentvolumesource is immutable after creation"));
    }
    if old.pointer("/spec/volumeMode") != new.pointer("/spec/volumeMode") {
        errs.push(FieldError::invalid("spec.volumeMode", str_at(new, "/spec/volumeMode"), "field is immutable"));
    }
    errs
}

fn pv_columns() -> Vec<Column> {
    vec![
        Column::new("Capacity", "string", "capacity is the description of the persistent volume's resources and capacity."),
        Column::new("Access Modes", "string", "accessModes contains all ways the volume can be mounted."),
        Column::new(
            "Reclaim Policy",
            "string",
            "persistentVolumeReclaimPolicy defines what happens to a persistent volume when released from its claim.",
        ),
        Column::new("Status", "string", "phase indicates if a volume is available, bound to a claim, or released by a claim."),
        Column::new("Claim", "string", "claimRef is part of a bi-directional binding between PersistentVolume and PersistentVolumeClaim."),
        Column::new("StorageClass", "string", "StorageClass of the pv"),
        Column::new("VolumeAttributesClass", "string", "VolumeAttributesClass of the pv"),
        Column::new("Reason", "string", "reason is a brief CamelCase string that describes any failure."),
        Column::new("Age", "string", ""),
        Column::new(
            "VolumeMode",
            "string",
            "volumeMode defines if a volume is intended to be used with a formatted filesystem or to remain in raw block state.",
        )
        .wide(),
    ]
}

fn pv_cells(obj: &Value) -> Vec<Value> {
    let claim = obj
        .pointer("/spec/claimRef")
        .filter(|c| c.is_object())
        .map(|c| format!("{}/{}", str_at(c, "/namespace"), str_at(c, "/name")))
        .unwrap_or_default();
    let phase =
        if obj.pointer("/metadata/deletionTimestamp").is_some_and(|d| !d.is_null()) { "Terminating" } else { str_at(obj, "/status/phase") };
    let mode = obj.pointer("/spec/volumeMode").and_then(Value::as_str).unwrap_or("<unset>");
    let vac = obj.pointer("/spec/volumeAttributesClassName").and_then(Value::as_str).unwrap_or("<unset>");
    vec![
        json!(str_at(obj, "/spec/capacity/storage")),
        json!(access_modes_string(obj.pointer("/spec/accessModes"))),
        json!(str_at(obj, "/spec/persistentVolumeReclaimPolicy")),
        json!(phase),
        json!(claim),
        json!(class_of(obj)),
        json!(vac),
        json!(str_at(obj, "/status/reason")),
        age_cell(obj),
        json!(mode),
    ]
}

// ---------------------------------------------------------------------------
// PersistentVolumeClaim
// ---------------------------------------------------------------------------

pub fn persistent_volume_claim() -> SimpleStrategy {
    SimpleStrategy::new(true)
        .defaults(pvc_defaults)
        .validate(pvc_validate)
        .validate_update(pvc_validate_update)
        .table(pvc_columns, pvc_cells)
}

/// `SetDefaults_PersistentVolumeClaimSpec` + `PrepareForCreate` (phase `Pending`).
fn pvc_defaults(obj: &mut Value) {
    if let Some(spec) = obj.get_mut("spec").and_then(Value::as_object_mut) {
        spec.entry("volumeMode").or_insert(json!("Filesystem"));
    }
    if obj.pointer("/status/phase").is_none() {
        obj["status"] = json!({"phase": "Pending"});
    }
}

fn pvc_validate(obj: &Value) -> ErrorList {
    let mut errs = vec![];
    validate_modes(obj, "spec.accessModes", &mut errs);
    match obj.pointer("/spec/resources/requests/storage").and_then(Value::as_str) {
        None => errs.push(FieldError::required("spec.resources[storage]", "")),
        Some(q) if !valid_quantity(q) => errs.push(FieldError::invalid("spec.resources[storage]", q, "must be a valid quantity")),
        Some(q) if k8s_apimachinery::quantity::parse_value(q) == Ok(0) => {
            errs.push(FieldError::invalid("spec.resources[storage]", q, "must be greater than zero"))
        }
        _ => {}
    }
    errs
}

/// `ValidatePersistentVolumeClaimUpdate`: only the storage request may change; the
/// volume name may be set once, and a missing class may be filled in (default class).
fn pvc_validate_update(new: &Value, old: &Value) -> ErrorList {
    let mut errs = pvc_validate(new);
    let mut a = old.get("spec").cloned().unwrap_or(json!({}));
    let mut b = new.get("spec").cloned().unwrap_or(json!({}));
    for spec in [&mut a, &mut b] {
        if let Some(m) = spec.as_object_mut() {
            m.remove("resources");
            m.remove("volumeAttributesClassName");
        }
    }
    if str_at(old, "/spec/volumeName").is_empty() {
        a["volumeName"] = b.get("volumeName").cloned().unwrap_or(Value::Null);
    }
    if old.pointer("/spec/storageClassName").is_none_or(Value::is_null) {
        a["storageClassName"] = b.get("storageClassName").cloned().unwrap_or(Value::Null);
    }
    let strip = |v: &mut Value| {
        if let Some(m) = v.as_object_mut() {
            m.retain(|_, x| !x.is_null());
        }
    };
    strip(&mut a);
    strip(&mut b);
    if a != b {
        errs.push(FieldError::forbidden(
            "spec",
            "spec is immutable after creation except resources.requests and volumeAttributesClassName for bound claims",
        ));
    }
    errs
}

fn pvc_columns() -> Vec<Column> {
    vec![
        Column::new("Status", "string", "phase represents the current phase of PersistentVolumeClaim."),
        Column::new("Volume", "string", "volumeName is the binding reference to the PersistentVolume backing this claim."),
        Column::new("Capacity", "string", "capacity represents the actual resources of the underlying volume."),
        Column::new("Access Modes", "string", "accessModes contains the actual access modes the volume backing the PVC has."),
        Column::new("StorageClass", "string", "StorageClass of the pvc"),
        Column::new("VolumeAttributesClass", "string", "VolumeAttributesClass of the pvc"),
        Column::new("Age", "string", ""),
        Column::new("VolumeMode", "string", "volumeMode defines what type of volume is required by the claim.").wide(),
    ]
}

fn pvc_cells(obj: &Value) -> Vec<Value> {
    let phase =
        if obj.pointer("/metadata/deletionTimestamp").is_some_and(|d| !d.is_null()) { "Terminating" } else { str_at(obj, "/status/phase") };
    let bound = !str_at(obj, "/spec/volumeName").is_empty();
    let (cap, modes) = if bound {
        (str_at(obj, "/status/capacity/storage").to_string(), access_modes_string(obj.pointer("/status/accessModes")))
    } else {
        (String::new(), String::new())
    };
    let vac = obj.pointer("/spec/volumeAttributesClassName").and_then(Value::as_str).unwrap_or("<unset>");
    vec![
        json!(phase),
        json!(str_at(obj, "/spec/volumeName")),
        json!(cap),
        json!(modes),
        json!(class_of(obj)),
        json!(vac),
        age_cell(obj),
        json!(obj.pointer("/spec/volumeMode").and_then(Value::as_str).unwrap_or("<unset>")),
    ]
}

// ---------------------------------------------------------------------------
// StorageClass
// ---------------------------------------------------------------------------

pub fn storage_class() -> SimpleStrategy {
    SimpleStrategy::new(false).defaults(sc_defaults).validate(sc_validate).validate_update(sc_validate_update).table(sc_columns, sc_cells)
}

/// `SetDefaults_StorageClass`
fn sc_defaults(obj: &mut Value) {
    if let Some(m) = obj.as_object_mut() {
        m.entry("reclaimPolicy").or_insert(json!("Delete"));
        m.entry("volumeBindingMode").or_insert(json!("Immediate"));
    }
}

fn sc_validate(obj: &Value) -> ErrorList {
    let mut errs = vec![];
    if str_at(obj, "/provisioner").is_empty() {
        errs.push(FieldError::required("provisioner", ""));
    }
    let rp = str_at(obj, "/reclaimPolicy");
    if !rp.is_empty() && !["Delete", "Retain"].contains(&rp) {
        errs.push(FieldError::not_supported("reclaimPolicy", rp, &["Delete", "Retain"]));
    }
    let bm = str_at(obj, "/volumeBindingMode");
    if !bm.is_empty() && !["Immediate", "WaitForFirstConsumer"].contains(&bm) {
        errs.push(FieldError::not_supported("volumeBindingMode", bm, &["Immediate", "WaitForFirstConsumer"]));
    }
    errs
}

/// `ValidateStorageClassUpdate`
fn sc_validate_update(new: &Value, old: &Value) -> ErrorList {
    let mut errs = sc_validate(new);
    if new.get("parameters") != old.get("parameters") {
        errs.push(FieldError::forbidden("parameters", "updates to parameters are forbidden."));
    }
    if new.get("provisioner") != old.get("provisioner") {
        errs.push(FieldError::forbidden("provisioner", "updates to provisioner are forbidden."));
    }
    if new.get("reclaimPolicy") != old.get("reclaimPolicy") {
        errs.push(FieldError::forbidden("reclaimPolicy", "updates to reclaimPolicy are forbidden."));
    }
    if new.get("volumeBindingMode") != old.get("volumeBindingMode") {
        errs.push(FieldError::forbidden("volumeBindingMode", "field is immutable"));
    }
    errs
}

fn sc_columns() -> Vec<Column> {
    vec![
        Column::new("Provisioner", "string", "provisioner indicates the type of the provisioner."),
        Column::new(
            "ReclaimPolicy",
            "string",
            "reclaimPolicy controls the reclaimPolicy for dynamically provisioned PersistentVolumes of this storage class.",
        ),
        Column::new(
            "VolumeBindingMode",
            "string",
            "volumeBindingMode indicates how PersistentVolumeClaims should be provisioned and bound.",
        ),
        Column::new("AllowVolumeExpansion", "string", "allowVolumeExpansion shows whether the storage class allow volume expand."),
        Column::new("Age", "string", ""),
    ]
}

fn sc_cells(obj: &Value) -> Vec<Value> {
    vec![
        json!(storage_class_display_name(obj).unwrap_or_else(|| str_at(obj, "/metadata/name").to_string())),
        json!(str_at(obj, "/provisioner")),
        json!(obj.get("reclaimPolicy").and_then(Value::as_str).unwrap_or("Delete")),
        json!(obj.get("volumeBindingMode").and_then(Value::as_str).unwrap_or("Immediate")),
        json!(obj.get("allowVolumeExpansion").and_then(Value::as_bool).unwrap_or(false).to_string()),
        age_cell(obj),
    ]
}

/// The printer's name cell gets ` (default)` for a default class.
pub fn storage_class_display_name(obj: &Value) -> Option<String> {
    let a = obj.pointer("/metadata/annotations");
    let is_default = ["storageclass.kubernetes.io/is-default-class", "storageclass.beta.kubernetes.io/is-default-class"]
        .iter()
        .any(|k| a.and_then(|a| a.get(*k)).and_then(Value::as_str) == Some("true"));
    is_default.then(|| format!("{} (default)", str_at(obj, "/metadata/name")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pv_rules_and_printer() {
        let mut pv = json!({"metadata": {"name": "pv1"}, "spec": {"capacity": {"storage": "1Gi"}, "accessModes": ["ReadWriteOnce", "ReadOnlyMany"],
            "hostPath": {"path": "/data"}, "claimRef": {"namespace": "ns", "name": "c"}}});
        pv_defaults(&mut pv);
        assert_eq!(pv["status"]["phase"], "Pending");
        assert_eq!(pv["spec"]["persistentVolumeReclaimPolicy"], "Retain");
        assert!(pv_validate(&pv).is_empty());
        let cells = pv_cells(&pv);
        assert_eq!(cells[1], "RWO,ROX");
        assert_eq!(cells[4], "ns/c");
        let mut moved = pv.clone();
        moved["spec"]["hostPath"]["path"] = json!("/other");
        assert!(pv_validate_update(&moved, &pv).iter().any(|e| e.message().contains("immutable after creation")));
        let local = json!({"spec": {"capacity": {"storage": "1Gi"}, "accessModes": ["ReadWriteOnce"], "local": {"path": "/mnt"}}});
        assert!(pv_validate(&local).iter().any(|e| e.message().contains("Local volume requires node affinity")));
        assert!(pv_validate(&json!({"spec": {"capacity": {"storage": "1Gi"}, "accessModes": ["ReadWriteOnce"]}}))
            .iter()
            .any(|e| e.message().contains("must specify a volume type")));
    }

    #[test]
    fn pvc_rules_and_printer() {
        let mut c =
            json!({"metadata": {"name": "c"}, "spec": {"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "1Gi"}}}});
        pvc_defaults(&mut c);
        assert!(pvc_validate(&c).is_empty());
        let mut bound = c.clone();
        bound["spec"]["volumeName"] = json!("pv1");
        bound["spec"]["storageClassName"] = json!("standard");
        bound["status"] = json!({"phase": "Bound", "capacity": {"storage": "1Gi"}, "accessModes": ["ReadWriteOnce"]});
        assert!(pvc_validate_update(&bound, &c).is_empty(), "volumeName once, class from nil");
        let mut changed = bound.clone();
        changed["spec"]["accessModes"] = json!(["ReadWriteMany"]);
        assert!(!pvc_validate_update(&changed, &bound).is_empty());
        let mut grow = bound.clone();
        grow["spec"]["resources"]["requests"]["storage"] = json!("2Gi");
        assert!(pvc_validate_update(&grow, &bound).is_empty());
        assert_eq!(pvc_cells(&bound)[..4], [json!("Bound"), json!("pv1"), json!("1Gi"), json!("RWO")]);
        assert_eq!(pvc_cells(&c)[2], "");
        assert!(pvc_validate(&json!({"spec": {"accessModes": ["ReadWriteOnce"]}})).iter().any(|e| e.field == "spec.resources[storage]"));
    }

    #[test]
    fn storage_class_rules() {
        let mut sc = json!({"metadata": {"name": "standard", "annotations": {"storageclass.kubernetes.io/is-default-class": "true"}}, "provisioner": "kubernetes.io/host-path"});
        sc_defaults(&mut sc);
        assert_eq!(sc["reclaimPolicy"], "Delete");
        assert_eq!(sc["volumeBindingMode"], "Immediate");
        assert_eq!(storage_class_display_name(&sc).as_deref(), Some("standard (default)"));
        let mut changed = sc.clone();
        changed["parameters"] = json!({"a": "b"});
        assert!(sc_validate_update(&changed, &sc).iter().any(|e| e.message().contains("updates to parameters are forbidden")));
    }
}
