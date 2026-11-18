//! Kubernetes protobuf wire format (`apimachinery/pkg/runtime/serializer/protobuf`).
//!
//! client-go's generated clients for built-in types always send request bodies
//! as `application/vnd.kubernetes.protobuf`. The format is:
//!
//! ```text
//! "k8s\x00" | runtime.Unknown{ typeMeta{apiVersion, kind}, raw: <message bytes> }
//! ```
//!
//! [`decode`] turns such a body into the same JSON the API server would have
//! received with `application/json`, using descriptors compiled from the
//! upstream `.proto` files and fixing up the special types (Time, Quantity,
//! IntOrString, RawExtension) whose JSON form differs from their protobuf form.

use std::sync::OnceLock;

use prost_reflect::{DescriptorPool, DynamicMessage, Kind, MessageDescriptor, SerializeOptions};
use serde_json::Value;

pub const MEDIA_TYPE: &str = "application/vnd.kubernetes.protobuf";
const MAGIC: &[u8] = b"k8s\x00";

static POOL: OnceLock<DescriptorPool> = OnceLock::new();

fn pool() -> &'static DescriptorPool {
    POOL.get_or_init(|| {
        DescriptorPool::decode(&include_bytes!(concat!(env!("OUT_DIR"), "/k8s_descriptors.bin"))[..])
            .expect("embedded Kubernetes descriptors")
    })
}

/// Proto package for an apiVersion (`v1` → `k8s.io.api.core.v1`).
fn package_for(api_version: &str) -> String {
    match api_version.split_once('/') {
        None => format!("k8s.io.api.core.{api_version}"),
        Some(("meta.k8s.io", v)) => format!("k8s.io.apimachinery.pkg.apis.meta.{v}"),
        Some((group, v)) => format!("k8s.io.api.{}.{v}", group.split('.').next().unwrap_or(group)),
    }
}

fn find_message(api_version: &str, kind: &str) -> Option<MessageDescriptor> {
    let version = api_version.rsplit('/').next().unwrap_or("v1");
    pool()
        .get_message_by_name(&format!("{}.{kind}", package_for(api_version)))
        // DeleteOptions, ListOptions, Status... are registered in every group version.
        .or_else(|| pool().get_message_by_name(&format!("k8s.io.apimachinery.pkg.apis.meta.{version}.{kind}")))
}

/// True if the content type is the Kubernetes protobuf media type.
pub fn is_protobuf(content_type: &str) -> bool {
    content_type.split(';').next().unwrap_or("").trim() == MEDIA_TYPE
}

/// Decodes a Kubernetes protobuf body into JSON (with `apiVersion` and `kind` set).
pub fn decode(body: &[u8]) -> Result<Value, String> {
    let rest = body.strip_prefix(MAGIC).ok_or("protobuf body does not start with the k8s magic number")?;
    let unknown_desc = pool().get_message_by_name("k8s.io.apimachinery.pkg.runtime.Unknown").ok_or("runtime.Unknown descriptor missing")?;
    let unknown = DynamicMessage::decode(unknown_desc, rest).map_err(|e| format!("decoding runtime.Unknown: {e}"))?;
    let type_meta = unknown.get_field_by_name("typeMeta").ok_or("missing typeMeta")?;
    let tm = type_meta.as_message().ok_or("invalid typeMeta")?;
    let api_version = tm.get_field_by_name("apiVersion").and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
    let kind = tm.get_field_by_name("kind").and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
    let raw = unknown.get_field_by_name("raw").and_then(|v| v.as_bytes().map(|b| b.to_vec())).unwrap_or_default();
    let desc = find_message(&api_version, &kind).ok_or_else(|| format!("no protobuf schema for {api_version}, Kind={kind}"))?;
    let mut json = decode_message(&desc, &raw)?;
    if let Some(m) = json.as_object_mut() {
        m.insert("apiVersion".into(), Value::String(api_version));
        m.insert("kind".into(), Value::String(kind));
    }
    Ok(json)
}

/// Decodes raw message bytes of a given type into JSON.
pub fn decode_message(desc: &MessageDescriptor, raw: &[u8]) -> Result<Value, String> {
    let msg = DynamicMessage::decode(desc.clone(), raw).map_err(|e| format!("decoding {}: {e}", desc.full_name()))?;
    let opts = SerializeOptions::new().stringify_64_bit_integers(false).use_proto_field_name(true);
    let mut json = msg
        .serialize_with_options(serde_json::value::Serializer, &opts)
        .map_err(|e| format!("converting {} to JSON: {e}", desc.full_name()))?;
    fixup(&mut json, desc);
    Ok(json)
}

/// Converts special apimachinery types from their protobuf JSON shape to the Kubernetes JSON shape.
fn convert_special(name: &str, v: &mut Value) -> bool {
    match name {
        "k8s.io.apimachinery.pkg.apis.meta.v1.Time" | "k8s.io.apimachinery.pkg.apis.meta.v1.MicroTime" => {
            let secs = v.get("seconds").and_then(Value::as_i64).unwrap_or(0);
            let nanos = v.get("nanos").and_then(Value::as_i64).unwrap_or(0) as u32;
            if secs == 0 && nanos == 0 {
                // Go encodes the zero Time (non-pointer field); its JSON form is null.
                *v = Value::Null;
                return true;
            }
            let t = chrono::DateTime::from_timestamp(secs, nanos).unwrap_or_default();
            *v = Value::String(if name.ends_with("MicroTime") {
                t.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
            } else {
                t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            });
            true
        }
        "k8s.io.apimachinery.pkg.apis.meta.v1.Duration" => {
            let ns = v.get("duration").and_then(Value::as_i64).unwrap_or(0);
            *v = Value::String(format!("{}s", ns as f64 / 1e9));
            true
        }
        "k8s.io.apimachinery.pkg.api.resource.Quantity" => {
            *v = v.get("string").cloned().unwrap_or(Value::String("0".into()));
            true
        }
        "k8s.io.apimachinery.pkg.util.intstr.IntOrString" => {
            let is_str = v.get("type").and_then(Value::as_i64) == Some(1);
            *v = if is_str {
                v.get("strVal").cloned().unwrap_or(Value::String(String::new()))
            } else {
                v.get("intVal").cloned().unwrap_or(Value::from(0))
            };
            true
        }
        "k8s.io.apimachinery.pkg.runtime.RawExtension" | "k8s.io.apimachinery.pkg.apis.meta.v1.FieldsV1" => {
            let key = if name.ends_with("FieldsV1") { "Raw" } else { "raw" };
            let raw = v.get(key).and_then(Value::as_str).unwrap_or("");
            use base64::Engine;
            *v = base64::engine::general_purpose::STANDARD
                .decode(raw)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or(Value::Null);
            true
        }
        _ => false,
    }
}

fn fixup_value(v: &mut Value, desc: &MessageDescriptor) {
    if !convert_special(desc.full_name(), v) {
        fixup(v, desc);
    }
}

fn fixup(v: &mut Value, desc: &MessageDescriptor) {
    let Some(obj) = v.as_object_mut() else { return };
    for field in desc.fields() {
        // Go's generated marshaller writes non-pointer string fields even when empty,
        // while its JSON encoder omits them (`omitempty`). Drop them so defaulting applies.
        if matches!(field.kind(), Kind::String) && !field.is_list() && !field.is_map() {
            if obj.get(field.name()).and_then(Value::as_str) == Some("") {
                obj.remove(field.name());
            }
            continue;
        }
        let Some(fv) = obj.get_mut(field.name()) else { continue };
        let Kind::Message(md) = field.kind() else { continue };
        if field.is_map() {
            let Kind::Message(vd) = md.map_entry_value_field().kind() else { continue };
            if let Some(m) = fv.as_object_mut() {
                for (_, mv) in m.iter_mut() {
                    fixup_value(mv, &vd);
                }
            }
        } else if field.is_list() {
            if let Some(a) = fv.as_array_mut() {
                for item in a {
                    fixup_value(item, &md);
                }
            }
        } else {
            fixup_value(fv, &md);
        }
    }
    // Drop nulls left by special conversions (zero Time, invalid RawExtension).
    obj.retain(|_, v| !v.is_null());
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;
    use prost_reflect::Value as PValue;

    /// Builds a protobuf body the way client-go does, then decodes it.
    fn encode(api_version: &str, kind: &str, build: impl FnOnce(&mut DynamicMessage)) -> Vec<u8> {
        let desc = find_message(api_version, kind).unwrap();
        let mut msg = DynamicMessage::new(desc);
        build(&mut msg);
        let unknown_desc = pool().get_message_by_name("k8s.io.apimachinery.pkg.runtime.Unknown").unwrap();
        let tm_desc = pool().get_message_by_name("k8s.io.apimachinery.pkg.runtime.TypeMeta").unwrap();
        let mut tm = DynamicMessage::new(tm_desc);
        tm.set_field_by_name("apiVersion", PValue::String(api_version.into()));
        tm.set_field_by_name("kind", PValue::String(kind.into()));
        let mut unknown = DynamicMessage::new(unknown_desc);
        unknown.set_field_by_name("typeMeta", PValue::Message(tm));
        unknown.set_field_by_name("raw", PValue::Bytes(msg.encode_to_vec().into()));
        let mut out = MAGIC.to_vec();
        out.extend(unknown.encode_to_vec());
        out
    }

    #[test]
    fn decodes_configmap() {
        let body = encode("v1", "ConfigMap", |m| {
            let meta_desc = pool().get_message_by_name("k8s.io.apimachinery.pkg.apis.meta.v1.ObjectMeta").unwrap();
            let mut meta = DynamicMessage::new(meta_desc);
            meta.set_field_by_name("name", PValue::String("cfg".into()));
            meta.set_field_by_name("generation", PValue::I64(3));
            m.set_field_by_name("metadata", PValue::Message(meta));
            let mut data = std::collections::HashMap::new();
            data.insert(prost_reflect::MapKey::String("k".into()), PValue::String("v".into()));
            m.set_field_by_name("data", PValue::Map(data));
        });
        let v = decode(&body).unwrap();
        assert_eq!(v["kind"], "ConfigMap");
        assert_eq!(v["apiVersion"], "v1");
        assert_eq!(v["metadata"]["name"], "cfg");
        assert_eq!(v["metadata"]["generation"], 3);
        assert_eq!(v["data"]["k"], "v");
    }

    #[test]
    fn converts_special_types() {
        let body = encode("apps/v1", "Deployment", |m| {
            let spec_desc = pool().get_message_by_name("k8s.io.api.apps.v1.DeploymentSpec").unwrap();
            let mut spec = DynamicMessage::new(spec_desc);
            spec.set_field_by_name("replicas", PValue::I32(2));
            let strat_desc = pool().get_message_by_name("k8s.io.api.apps.v1.DeploymentStrategy").unwrap();
            let ru_desc = pool().get_message_by_name("k8s.io.api.apps.v1.RollingUpdateDeployment").unwrap();
            let ios_desc = pool().get_message_by_name("k8s.io.apimachinery.pkg.util.intstr.IntOrString").unwrap();
            let mut ios = DynamicMessage::new(ios_desc);
            ios.set_field_by_name("type", PValue::I64(1));
            ios.set_field_by_name("strVal", PValue::String("25%".into()));
            let mut ru = DynamicMessage::new(ru_desc);
            ru.set_field_by_name("maxSurge", PValue::Message(ios));
            let mut strat = DynamicMessage::new(strat_desc);
            strat.set_field_by_name("rollingUpdate", PValue::Message(ru));
            spec.set_field_by_name("strategy", PValue::Message(strat));
            m.set_field_by_name("spec", PValue::Message(spec));
            let meta_desc = pool().get_message_by_name("k8s.io.apimachinery.pkg.apis.meta.v1.ObjectMeta").unwrap();
            let time_desc = pool().get_message_by_name("k8s.io.apimachinery.pkg.apis.meta.v1.Time").unwrap();
            let mut t = DynamicMessage::new(time_desc);
            t.set_field_by_name("seconds", PValue::I64(1_700_000_000));
            let mut meta = DynamicMessage::new(meta_desc);
            meta.set_field_by_name("creationTimestamp", PValue::Message(t));
            m.set_field_by_name("metadata", PValue::Message(meta));
        });
        let v = decode(&body).unwrap();
        assert_eq!(v["spec"]["replicas"], 2);
        assert_eq!(v["spec"]["strategy"]["rollingUpdate"]["maxSurge"], "25%");
        assert_eq!(v["metadata"]["creationTimestamp"], "2023-11-14T22:13:20Z");
    }
}
