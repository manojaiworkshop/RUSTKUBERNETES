//! Port of `apiextensions-apiserver/pkg/apis/apiextensions/validation`
//! (`ValidateCustomResourceDefinition`, `...Update`, `...Status`) and the
//! structural-schema checks of `pkg/apiserver/schema/validation.go`.

use k8s_apimachinery::validation::{is_dns1035_label, is_dns1123_subdomain, ErrorList, FieldError};
use serde_json::Value;

use crate::apis::{self, str_at};
use crate::jsonpath::JsonPath;

const PRINTER_COLUMN_TYPES: &[&str] = &["boolean", "date", "integer", "number", "string"];
const PRINTER_COLUMN_FORMATS: &[&str] = &["byte", "date", "date-time", "double", "float", "int32", "int64", "password"];

/// `ValidateCustomResourceDefinition`
pub fn validate(crd: &Value) -> ErrorList {
    let mut errs = vec![];
    let name = str_at(crd, "/metadata/name");
    let required = format!("{}.{}", str_at(crd, "/spec/names/plural"), str_at(crd, "/spec/group"));
    if !name.is_empty() && name != required {
        errs.push(FieldError::invalid("metadata.name", name, r#"must be spec.names.plural+"."+spec.group"#));
    }
    errs.extend(validate_spec(&crd["spec"]));
    errs.extend(validate_stored_versions(crd));
    errs.extend(validate_api_approval(crd));
    errs
}

/// `ValidateCustomResourceDefinitionUpdate`
pub fn validate_update(new: &Value, old: &Value) -> ErrorList {
    let mut errs = validate(new);
    let immutable = |errs: &mut ErrorList, path: &str, ptr: &str| {
        let (n, o) = (str_at(new, ptr), str_at(old, ptr));
        if n != o {
            errs.push(FieldError::invalid(path, n, "field is immutable"));
        }
    };
    if apis::is_condition_true(old, apis::ESTABLISHED) {
        immutable(&mut errs, "spec.scope", "/spec/scope");
        immutable(&mut errs, "spec.names.kind", "/spec/names/kind");
    }
    immutable(&mut errs, "spec.group", "/spec/group");
    immutable(&mut errs, "spec.names.plural", "/spec/names/plural");
    errs
}

/// `ValidateUpdateCustomResourceDefinitionStatus`
pub fn validate_status(crd: &Value) -> ErrorList {
    let mut errs = vec![];
    let accepted = &crd["status"]["acceptedNames"];
    if accepted.is_object() {
        errs.extend(validate_names(accepted, "status.acceptedNames", false));
    }
    errs.extend(validate_stored_versions(crd));
    errs
}

fn validate_stored_versions(crd: &Value) -> ErrorList {
    let Some(stored) = crd.pointer("/status/storedVersions").and_then(Value::as_array) else { return vec![] };
    let stored: Vec<&str> = stored.iter().filter_map(Value::as_str).collect();
    let mut errs = vec![];
    if stored.is_empty() {
        return vec![FieldError::invalid_raw("status.storedVersions", "[]", "must have at least one stored version")];
    }
    if let Some(sv) = apis::storage_version(crd) {
        if !stored.contains(&sv.as_str()) {
            errs.push(FieldError::invalid_raw(
                "status.storedVersions",
                format!("{stored:?}"),
                format!("must have the storage version {sv}"),
            ));
        }
    }
    let names: Vec<String> = apis::versions(crd).iter().map(|v| str_at(v, "/name").to_string()).collect();
    for (i, v) in stored.iter().enumerate() {
        if !names.iter().any(|n| n == v) {
            errs.push(FieldError::invalid(
                format!("status.storedVersions[{i}]"),
                v,
                format!("missing from spec.versions; {v} was previously a storage version, and must remain in spec.versions until a storage migration ensures no data remains persisted in {v} and removes {v} from status.storedVersions"),
            ));
        }
    }
    errs
}

fn validate_api_approval(crd: &Value) -> ErrorList {
    if !apis::is_protected_group(str_at(crd, "/spec/group")) {
        return vec![];
    }
    let ann = crd.pointer("/metadata/annotations").cloned().unwrap_or(Value::Null);
    match apis::api_approval_state(&ann) {
        ("Missing", reason) => {
            vec![FieldError::required(format!("metadata.annotations[{}]", apis::API_APPROVAL_ANNOTATION), reason)]
        }
        ("Invalid", reason) => vec![FieldError::invalid(
            format!("metadata.annotations[{}]", apis::API_APPROVAL_ANNOTATION),
            ann.get(apis::API_APPROVAL_ANNOTATION).and_then(Value::as_str).unwrap_or(""),
            reason,
        )],
        _ => vec![],
    }
}

fn enum_string(errs: &mut ErrorList, path: &str, v: &str, accepted: &[&str], required: bool) {
    if v.is_empty() {
        if required {
            errs.push(FieldError::required(path, ""));
        }
        return;
    }
    if !accepted.contains(&v) {
        errs.push(FieldError::not_supported(path, v, accepted));
    }
}

fn validate_spec(spec: &Value) -> ErrorList {
    let mut errs = vec![];
    let group = str_at(spec, "/group");
    if group.is_empty() {
        errs.push(FieldError::required("spec.group", ""));
    } else {
        let e = is_dns1123_subdomain(group);
        if !e.is_empty() {
            errs.push(FieldError::invalid("spec.group", group, e.join(",")));
        } else if group.split('.').count() < 2 {
            errs.push(FieldError::invalid("spec.group", group, "should be a domain with at least one dot"));
        }
    }
    enum_string(&mut errs, "spec.scope", str_at(spec, "/scope"), &["Cluster", "Namespaced"], true);

    let versions = spec.get("versions").and_then(Value::as_array).cloned().unwrap_or_default();
    if versions.is_empty() {
        errs.push(FieldError::invalid_raw("spec.versions", "[]", "must have exactly one version marked as storage version"));
    }
    let mut storage = 0;
    let mut served = 0;
    let mut seen = std::collections::BTreeSet::new();
    let mut unique = true;
    for (i, v) in versions.iter().enumerate() {
        let p = format!("spec.versions[{i}]");
        let name = str_at(v, "/name");
        let e = is_dns1035_label(name);
        if !e.is_empty() {
            errs.push(FieldError::invalid(format!("{p}.name"), name, e.join(",")));
        }
        if !seen.insert(name.to_string()) {
            unique = false;
        }
        if v["storage"] == true {
            storage += 1;
        }
        if v["served"] == true {
            served += 1;
        }
        match v.pointer("/schema/openAPIV3Schema").filter(|s| s.is_object()) {
            None => errs.push(FieldError::required(format!("{p}.schema.openAPIV3Schema"), "")),
            Some(s) => errs.extend(validate_structural(s, &format!("{p}.schema.openAPIV3Schema"))),
        }
        for (j, col) in v.get("additionalPrinterColumns").and_then(Value::as_array).into_iter().flatten().enumerate() {
            errs.extend(validate_column(col, &format!("{p}.additionalPrinterColumns[{j}]")));
        }
        errs.extend(validate_subresources(v, &p));
    }
    if !versions.is_empty() {
        if !unique {
            errs.push(FieldError::invalid_raw("spec.versions", "...", "must contain unique version names"));
        }
        if storage != 1 {
            errs.push(FieldError::invalid_raw("spec.versions", "...", "must have exactly one version marked as storage version"));
        }
        if served == 0 {
            errs.push(FieldError::invalid_raw("spec.versions", "...", "must have at least one version marked as served"));
        }
    }
    errs.extend(validate_names(&spec["names"], "spec.names", true));
    // conversion
    let strategy = str_at(spec, "/conversion/strategy");
    enum_string(&mut errs, "spec.conversion.strategy", strategy, &["None", "Webhook"], true);
    let webhook = spec.pointer("/conversion/webhook").filter(|w| w.is_object());
    if strategy == "Webhook" {
        match webhook.and_then(|w| w.get("clientConfig")).filter(|c| c.is_object()) {
            None => errs.push(FieldError::required("spec.conversion.webhookClientConfig", "required when strategy is set to Webhook")),
            Some(cc) => {
                let has_url = !str_at(cc, "/url").is_empty();
                let has_svc = cc.get("service").is_some_and(Value::is_object);
                if has_url == has_svc {
                    errs.push(FieldError::required("spec.conversion.webhookClientConfig", "exactly one of url or service is required"));
                }
            }
        }
        let crv: Vec<&str> = webhook
            .and_then(|w| w.get("conversionReviewVersions"))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if crv.is_empty() {
            errs.push(FieldError::required("spec.conversion.conversionReviewVersions", ""));
        } else if !crv.iter().any(|v| *v == "v1" || *v == "v1beta1") {
            errs.push(FieldError::invalid_raw(
                "spec.conversion.conversionReviewVersions",
                format!("[]string{{{}}}", crv.iter().map(|v| format!("{v:?}")).collect::<Vec<_>>().join(", ")),
                "must include at least one of v1, v1beta1",
            ));
        }
    } else if webhook.is_some_and(|w| w.get("clientConfig").is_some_and(Value::is_object)) {
        errs.push(FieldError::forbidden("spec.conversion.webhookClientConfig", "should not be set when strategy is not set to Webhook"));
    }
    if spec["preserveUnknownFields"] == true {
        errs.push(FieldError::invalid_raw(
            "spec.preserveUnknownFields",
            "true",
            "cannot set to true, set x-kubernetes-preserve-unknown-fields to true in spec.versions[*].schema instead",
        ));
    }
    errs
}

fn validate_names(names: &Value, path: &str, required: bool) -> ErrorList {
    let mut errs = vec![];
    let get = |k: &str| names.get(k).and_then(Value::as_str).unwrap_or("");
    for k in ["plural", "singular", "kind", "listKind"] {
        if required && get(k).is_empty() {
            errs.push(FieldError::required(format!("{path}.{k}"), ""));
        }
    }
    let label = |errs: &mut ErrorList, k: &str, v: &str, lower: bool, prefix: &str| {
        if v.is_empty() {
            return;
        }
        let e = is_dns1035_label(&if lower { v.to_lowercase() } else { v.to_string() });
        if !e.is_empty() {
            errs.push(FieldError::invalid(format!("{path}.{k}"), v, format!("{prefix}{}", e.join(","))));
        }
    };
    label(&mut errs, "plural", get("plural"), false, "");
    label(&mut errs, "singular", get("singular"), false, "");
    label(&mut errs, "kind", get("kind"), true, "may have mixed case, but should otherwise match: ");
    label(&mut errs, "listKind", get("listKind"), true, "may have mixed case, but should otherwise match: ");
    for (i, sn) in names.get("shortNames").and_then(Value::as_array).into_iter().flatten().enumerate() {
        let sn = sn.as_str().unwrap_or("");
        let e = is_dns1035_label(sn);
        if !e.is_empty() {
            errs.push(FieldError::invalid(format!("{path}.shortNames[{i}]"), sn, e.join(",")));
        }
    }
    if !get("kind").is_empty() && get("kind") == get("listKind") {
        errs.push(FieldError::invalid(format!("{path}.listKind"), get("listKind"), "kind and listKind may not be the same"));
    }
    for (i, c) in names.get("categories").and_then(Value::as_array).into_iter().flatten().enumerate() {
        let c = c.as_str().unwrap_or("");
        let e = is_dns1035_label(c);
        if !e.is_empty() {
            errs.push(FieldError::invalid(format!("{path}.categories[{i}]"), c, e.join(",")));
        }
    }
    errs
}

fn validate_column(col: &Value, path: &str) -> ErrorList {
    let mut errs = vec![];
    if str_at(col, "/name").is_empty() {
        errs.push(FieldError::required(format!("{path}.name"), ""));
    }
    let ty = str_at(col, "/type");
    let types = format!("must be one of {}", PRINTER_COLUMN_TYPES.join(","));
    if ty.is_empty() {
        errs.push(FieldError::required(format!("{path}.type"), types));
    } else if !PRINTER_COLUMN_TYPES.contains(&ty) {
        errs.push(FieldError::invalid(format!("{path}.type"), ty, types));
    }
    let format = str_at(col, "/format");
    if !format.is_empty() && !PRINTER_COLUMN_FORMATS.contains(&format) {
        errs.push(FieldError::invalid(format!("{path}.format"), format, format!("must be one of {}", PRINTER_COLUMN_FORMATS.join(","))));
    }
    let jp = str_at(col, "/jsonPath");
    if jp.is_empty() {
        errs.push(FieldError::required(format!("{path}.JSONPath"), ""));
    } else if let Err(e) = JsonPath::parse(jp) {
        errs.push(FieldError::invalid(format!("{path}.JSONPath"), jp, format!("is an invalid path: {e}")));
    }
    errs
}

/// Simple dotted JSON paths of the scale subresource (`.spec.replicas`).
fn validate_simple_json_path(errs: &mut ErrorList, path: &str, v: &str, required: bool, under: &[&str]) {
    if v.is_empty() {
        if required {
            errs.push(FieldError::required(path, ""));
        }
        return;
    }
    let ok = v.starts_with('.') && v[1..].split('.').all(|p| !p.is_empty() && !p.contains(['[', ']', '*']));
    if !ok {
        errs.push(FieldError::invalid(path, v, "must be a simple json path starting with ."));
        return;
    }
    if !under.iter().any(|u| v.starts_with(u)) {
        errs.push(FieldError::invalid(path, v, format!("should be a json path under {}", under.join(" or "))));
    }
}

fn validate_subresources(v: &Value, p: &str) -> ErrorList {
    let mut errs = vec![];
    if let Some(scale) = v.pointer("/subresources/scale").filter(|s| s.is_object()) {
        let sp = format!("{p}.subresources.scale");
        validate_simple_json_path(&mut errs, &format!("{sp}.specReplicasPath"), str_at(scale, "/specReplicasPath"), true, &[".spec."]);
        validate_simple_json_path(
            &mut errs,
            &format!("{sp}.statusReplicasPath"),
            str_at(scale, "/statusReplicasPath"),
            true,
            &[".status."],
        );
        validate_simple_json_path(
            &mut errs,
            &format!("{sp}.labelSelectorPath"),
            str_at(scale, "/labelSelectorPath"),
            false,
            &[".spec.", ".status."],
        );
    }
    errs
}

/// The structural schema rules (`ValidateStructural` + root restrictions).
pub fn validate_structural(root: &Value, path: &str) -> ErrorList {
    let mut errs = vec![];
    structural_rec(root, path, true, false, &mut errs);
    if str_at(root, "/type") != "object" && !str_at(root, "/type").is_empty() {
        errs.push(FieldError::invalid(format!("{path}.type"), str_at(root, "/type"), "must be object at the root"));
    }
    if root["nullable"] == true {
        errs.push(FieldError::forbidden(format!("{path}.nullable"), "nullable cannot be true at the root"));
    }
    if root.get("additionalProperties").is_some() {
        errs.push(FieldError::forbidden(format!("{path}.additionalProperties"), "must not be used at the root"));
    }
    if let Some(meta) = root.pointer("/properties/metadata") {
        let extra: Vec<&String> = meta
            .get("properties")
            .and_then(Value::as_object)
            .map(|m| m.keys().filter(|k| *k != "name" && *k != "generateName").collect())
            .unwrap_or_default();
        let other_keys = meta.as_object().is_some_and(|m| m.keys().any(|k| !matches!(k.as_str(), "type" | "properties" | "description")));
        if !extra.is_empty() || other_keys {
            errs.push(FieldError::forbidden(
                format!("{path}.properties[metadata]"),
                "must not specify anything other than name and generateName, but metadata is implicitly specified",
            ));
        }
    }
    errs
}

fn structural_rec(s: &Value, path: &str, root: bool, items_ctx: bool, errs: &mut ErrorList) {
    let ty = str_at(s, "/type");
    let int_or_string = s["x-kubernetes-int-or-string"] == true;
    let preserve = s["x-kubernetes-preserve-unknown-fields"] == true;
    let embedded = s["x-kubernetes-embedded-resource"] == true;
    if s.get("x-kubernetes-preserve-unknown-fields").is_some_and(|v| v == false) {
        errs.push(FieldError::invalid_raw(format!("{path}.x-kubernetes-preserve-unknown-fields"), "false", "must be true or undefined"));
    }
    if ty.is_empty() && !int_or_string && (root || !preserve) {
        let why = if root {
            "must not be empty at the root"
        } else if items_ctx {
            "must not be empty for specified array items"
        } else {
            "must not be empty for specified object fields"
        };
        errs.push(FieldError::required(format!("{path}.type"), why));
    }
    if !ty.is_empty() && !["array", "boolean", "integer", "number", "object", "string"].contains(&ty) {
        errs.push(FieldError::not_supported(format!("{path}.type"), ty, &["array", "boolean", "integer", "number", "object", "string"]));
    }
    if int_or_string && preserve {
        errs.push(FieldError::invalid_raw(
            format!("{path}.x-kubernetes-preserve-unknown-fields"),
            "true",
            "must be false if x-kubernetes-int-or-string is true",
        ));
    }
    if embedded && ty != "object" {
        if ty.is_empty() {
            errs.push(FieldError::required(format!("{path}.type"), "must be object if x-kubernetes-embedded-resource is true"));
        } else {
            errs.push(FieldError::invalid(format!("{path}.type"), ty, "must be object if x-kubernetes-embedded-resource is true"));
        }
    }
    if ty == "array" && s.get("items").is_none() {
        errs.push(FieldError::required(format!("{path}.items"), "must be specified"));
    }
    if s.get("properties").is_some() && s.get("additionalProperties").is_some_and(|a| a.is_object()) {
        errs.push(FieldError::forbidden(
            format!("{path}.additionalProperties"),
            "additionalProperties and properties are mutual exclusive",
        ));
    }
    if let Some(p) = s.get("pattern").and_then(Value::as_str) {
        if let Err(e) = regex::Regex::new(p) {
            errs.push(FieldError::invalid(format!("{path}.pattern"), p, format!("must be a valid regular expression, but isn't: {e}")));
        }
    }
    if let Some(lt) = s.get("x-kubernetes-list-type").and_then(Value::as_str) {
        if !["atomic", "set", "map"].contains(&lt) {
            errs.push(FieldError::not_supported(format!("{path}.x-kubernetes-list-type"), lt, &["atomic", "set", "map"]));
        }
        if ty != "array" {
            errs.push(FieldError::invalid(format!("{path}.type"), ty, "must be array if x-kubernetes-list-type is specified"));
        }
        if lt == "map" && s.get("x-kubernetes-list-map-keys").and_then(Value::as_array).is_none_or(|k| k.is_empty()) {
            errs.push(FieldError::required(
                format!("{path}.x-kubernetes-list-map-keys"),
                "must not be empty if x-kubernetes-list-type is map",
            ));
        }
    }
    if let Some(props) = s.get("properties").and_then(Value::as_object) {
        for (k, v) in props {
            structural_rec(v, &format!("{path}.properties[{k}]"), false, false, errs);
        }
    }
    if let Some(it) = s.get("items").filter(|i| i.is_object()) {
        structural_rec(it, &format!("{path}.items"), false, true, errs);
    }
    if let Some(ap) = s.get("additionalProperties").filter(|a| a.is_object()) {
        structural_rec(ap, &format!("{path}.additionalProperties"), false, false, errs);
    }
    // Value validations may not specify types inside logical junctors.
    for junctor in ["allOf", "anyOf", "oneOf"] {
        for (i, alt) in s.get(junctor).and_then(Value::as_array).into_iter().flatten().enumerate() {
            if alt.get("type").is_some() {
                errs.push(FieldError::forbidden(format!("{path}.{junctor}[{i}].type"), "must be empty to be structural"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn crd() -> Value {
        json!({"metadata": {"name": "widgets.example.com"}, "spec": {
            "group": "example.com", "scope": "Namespaced",
            "names": {"plural": "widgets", "singular": "widget", "kind": "Widget", "listKind": "WidgetList"},
            "conversion": {"strategy": "None"},
            "versions": [{"name": "v1", "served": true, "storage": true,
                "schema": {"openAPIV3Schema": {"type": "object", "properties": {"spec": {"type": "object", "properties": {"size": {"type": "integer"}}}}}}}]}})
    }

    fn msgs(e: &ErrorList) -> Vec<String> {
        e.iter().map(|e| e.to_string()).collect()
    }

    #[test]
    fn valid() {
        assert_eq!(msgs(&validate(&crd())), Vec::<String>::new());
    }

    #[test]
    fn invalid() {
        let mut c = crd();
        c["metadata"]["name"] = json!("foo.example.com");
        c["spec"]["group"] = json!("example");
        c["spec"]["scope"] = json!("Global");
        c["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["properties"]["size"] = json!({});
        let m = msgs(&validate(&c));
        assert!(
            m.contains(&r#"metadata.name: Invalid value: "foo.example.com": must be spec.names.plural+"."+spec.group"#.into()),
            "{m:?}"
        );
        assert!(m.contains(&r#"spec.group: Invalid value: "example": should be a domain with at least one dot"#.into()), "{m:?}");
        assert!(m.contains(&r#"spec.scope: Unsupported value: "Global": supported values: "Cluster", "Namespaced""#.into()), "{m:?}");
        assert!(
            m.iter().any(
                |x| x.contains("properties[spec].properties[size].type: Required value: must not be empty for specified object fields")
            ),
            "{m:?}"
        );
        let mut k8s = crd();
        k8s["metadata"]["name"] = json!("widgets.x.k8s.io");
        k8s["spec"]["group"] = json!("x.k8s.io");
        assert!(msgs(&validate(&k8s))[0].starts_with("metadata.annotations[api-approved.kubernetes.io]: Required value"));
    }

    #[test]
    fn update() {
        let old = crd();
        let mut new = crd();
        new["spec"]["names"]["plural"] = json!("gadgets");
        new["metadata"]["name"] = json!("gadgets.example.com");
        assert!(msgs(&validate_update(&new, &old)).contains(&r#"spec.names.plural: Invalid value: "gadgets": field is immutable"#.into()));
    }
}
