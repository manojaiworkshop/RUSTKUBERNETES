//! CustomResourceDefinitions against a real in-process API server
//! (Go: `staging/src/k8s.io/apiextensions-apiserver/test/integration`).

use std::time::Duration;

use k8s_client_go::rest::Method;
use kube_integration_tests::{eventually, start_test_server};
use serde_json::{json, Value};

const T: Duration = Duration::from_secs(10);

fn crd() -> Value {
    json!({
        "apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
        "metadata": {"name": "widgets.example.com"},
        "spec": {
            "group": "example.com", "scope": "Namespaced",
            "names": {"plural": "widgets", "singular": "widget", "kind": "Widget", "shortNames": ["wd"], "categories": ["all"]},
            "versions": [
                {"name": "v1beta1", "served": true, "storage": false,
                 "schema": {"openAPIV3Schema": {"type": "object", "x-kubernetes-preserve-unknown-fields": true}}},
                {"name": "v1", "served": true, "storage": true,
                 "subresources": {"status": {}, "scale": {"specReplicasPath": ".spec.replicas", "statusReplicasPath": ".status.replicas", "labelSelectorPath": ".status.selector"}},
                 "additionalPrinterColumns": [
                    {"name": "Size", "type": "integer", "jsonPath": ".spec.size"},
                    {"name": "Color", "type": "string", "jsonPath": ".spec.color"},
                    {"name": "Age", "type": "date", "jsonPath": ".metadata.creationTimestamp"}],
                 "schema": {"openAPIV3Schema": {"type": "object", "properties": {
                    "spec": {"type": "object", "required": ["size"], "properties": {
                        "size": {"type": "integer", "minimum": 1, "maximum": 10},
                        "color": {"type": "string", "enum": ["red", "blue"], "default": "red"},
                        "replicas": {"type": "integer"}}},
                    "status": {"type": "object", "properties": {"replicas": {"type": "integer"}, "selector": {"type": "string"}}}}}}}
            ]
        }
    })
}

async fn established(c: &k8s_client_go::Client) -> Value {
    eventually("CRD established", T, || async {
        let crd = c.get_json("/apis/apiextensions.k8s.io/v1/customresourcedefinitions/widgets.example.com").await.ok()?;
        let conds = crd.pointer("/status/conditions")?.as_array()?.clone();
        conds.iter().any(|x| x["type"] == "Established" && x["status"] == "True").then_some(crd)
    })
    .await
}

#[tokio::test]
async fn crd_lifecycle() {
    let srv = start_test_server().await;
    let c = &srv.client;
    let crds = "/apis/apiextensions.k8s.io/v1/customresourcedefinitions";
    let created = c.send_json(Method::POST, crds, &crd()).await.unwrap();
    assert_eq!(created["spec"]["names"]["listKind"], "WidgetList");
    assert_eq!(created["status"]["storedVersions"], json!(["v1"]));
    let crd = established(c).await;
    assert_eq!(crd["status"]["acceptedNames"]["kind"], "Widget");
    assert!(crd["status"]["conditions"].as_array().unwrap().iter().any(|x| x["type"] == "NamesAccepted" && x["reason"] == "NoConflicts"));

    // discovery: preferred version is v1, resources carry short names, categories, subresources
    let g = c.get_json("/apis/example.com").await.unwrap();
    assert_eq!(g["preferredVersion"]["version"], "v1");
    let rl = c.get_json("/apis/example.com/v1").await.unwrap();
    let names: Vec<&str> = rl["resources"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["widgets", "widgets/status", "widgets/scale"]);
    assert_eq!(rl["resources"][0]["shortNames"], json!(["wd"]));
    eventually("APIService v1.example.com", T, || async {
        c.get_json("/apis/apiregistration.k8s.io/v1/apiservices/v1.example.com").await.ok()
    })
    .await;

    // create: defaulting, pruning (Warn) and Strict field validation
    let base = "/apis/example.com/v1/namespaces/default/widgets";
    let w = json!({"apiVersion": "example.com/v1", "kind": "Widget", "metadata": {"name": "w1"}, "spec": {"size": 3, "bogus": 1}});
    let (code, _, body) =
        c.raw(Method::POST, &format!("{base}?fieldValidation=Strict"), Some("application/json"), w.to_string().into_bytes()).await.unwrap();
    assert_eq!(code, 400);
    assert!(
        String::from_utf8_lossy(&body).contains(r#"strict decoding error: unknown field \"spec.bogus\""#),
        "{}",
        String::from_utf8_lossy(&body)
    );
    let w1 = c.send_json(Method::POST, base, &w).await.unwrap();
    assert_eq!(w1["spec"], json!({"size": 3, "color": "red"}));
    assert_eq!(w1["metadata"]["generation"], 1);

    // validation
    let bad = json!({"apiVersion": "example.com/v1", "kind": "Widget", "metadata": {"name": "w2"}, "spec": {"size": 20, "color": "green"}});
    let err = c.send_json(Method::POST, base, &bad).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("spec.size: Invalid value: 20: spec.size in body should be less than or equal to 10"), "{msg}");
    assert!(msg.contains(r#"spec.color: Unsupported value: "green": supported values: "red", "blue""#), "{msg}");

    // status subresource: the main endpoint ignores status
    let mut upd = w1.clone();
    upd["status"] = json!({"replicas": 5});
    upd["spec"]["size"] = json!(4);
    let after = c.send_json(Method::PUT, &format!("{base}/w1"), &upd).await.unwrap();
    assert!(after.get("status").is_none());
    assert_eq!(after["metadata"]["generation"], 2);
    let mut st = after.clone();
    st["status"] = json!({"replicas": 2, "selector": "app=w"});
    let after = c.send_json(Method::PUT, &format!("{base}/w1/status"), &st).await.unwrap();
    assert_eq!(after["status"]["replicas"], 2);
    assert_eq!(after["metadata"]["generation"], 2);

    // scale subresource
    let scale = c.get_json(&format!("{base}/w1/scale")).await.unwrap();
    assert_eq!(scale["spec"]["replicas"], 0);
    assert_eq!(scale["status"], json!({"replicas": 2, "selector": "app=w"}));
    let mut sc = scale.clone();
    sc["spec"]["replicas"] = json!(7);
    c.send_json(Method::PUT, &format!("{base}/w1/scale"), &sc).await.unwrap();
    assert_eq!(c.get_json(&format!("{base}/w1")).await.unwrap()["spec"]["replicas"], 7);

    // patches: strategic merge is not supported for custom resources
    let (code, _, _) =
        c.raw(Method::PATCH, &format!("{base}/w1"), Some("application/strategic-merge-patch+json"), b"{}".to_vec()).await.unwrap();
    assert_eq!(code, 415);
    let p = c
        .request_raw(Method::PATCH, &format!("{base}/w1"), "application/merge-patch+json", br#"{"spec":{"color":"blue"}}"#.to_vec())
        .await
        .unwrap();
    assert_eq!(p["spec"]["color"], "blue");

    // table printing with additionalPrinterColumns
    let t = c.get_with_accept(base, "application/json;as=Table;v=v1;g=meta.k8s.io").await.unwrap();
    let cols: Vec<&str> = t["columnDefinitions"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(cols, vec!["Name", "Size", "Color", "Age"]);
    assert_eq!(t["rows"][0]["cells"][1], 4);
    assert_eq!(t["rows"][0]["cells"][2], "blue");

    // multiple versions: stored as v1, served as v1beta1 too
    let b = c.get_json("/apis/example.com/v1beta1/namespaces/default/widgets/w1").await.unwrap();
    assert_eq!(b["apiVersion"], "example.com/v1beta1");
    let w3 = json!({"apiVersion": "example.com/v1beta1", "kind": "Widget", "metadata": {"name": "w3"}, "spec": {"anything": true}});
    c.send_json(Method::POST, "/apis/example.com/v1beta1/namespaces/default/widgets", &w3).await.unwrap();
    let raw = srv.server.storage.get("/registry/example.com/widgets/default/w3").await.unwrap();
    assert_eq!(raw["apiVersion"], "example.com/v1");
    let list = c.get_json(base).await.unwrap();
    assert_eq!(list["kind"], "WidgetList");
    assert_eq!(list["items"].as_array().unwrap().len(), 2);
    assert!(list["items"].as_array().unwrap().iter().all(|i| i["apiVersion"] == "example.com/v1"));

    // deleting the CRD deletes its instances, then the CRD and its endpoints go away
    c.request(Method::DELETE, &format!("{crds}/widgets.example.com"), None).await.unwrap();
    eventually("CRD removed", T, || async { c.get_json(&format!("{crds}/widgets.example.com")).await.err().filter(|e| e.is_not_found()) })
        .await;
    assert!(srv.server.storage.get("/registry/example.com/widgets/default/w1").await.is_err());
    assert!(c.get_json(base).await.unwrap_err().is_not_found());
    eventually("APIService removed", T, || async {
        c.get_json("/apis/apiregistration.k8s.io/v1/apiservices/v1.example.com").await.err().filter(|e| e.is_not_found())
    })
    .await;
}

#[tokio::test]
async fn crd_validation_and_names() {
    let srv = start_test_server().await;
    let c = &srv.client;
    let crds = "/apis/apiextensions.k8s.io/v1/customresourcedefinitions";
    let mut bad = crd();
    bad["metadata"]["name"] = json!("gadgets.example.com");
    bad["spec"]["scope"] = json!("Global");
    let msg = c.send_json(Method::POST, crds, &bad).await.unwrap_err().to_string();
    assert!(msg.contains(r#"metadata.name: Invalid value: "gadgets.example.com": must be spec.names.plural+"."+spec.group"#), "{msg}");
    assert!(msg.contains(r#"spec.scope: Unsupported value: "Global": supported values: "Cluster", "Namespaced""#), "{msg}");

    // a second CRD in the same group asking for a used kind is not established
    c.send_json(Method::POST, crds, &crd()).await.unwrap();
    established(c).await;
    let mut other = crd();
    other["metadata"]["name"] = json!("gizmos.example.com");
    other["spec"]["names"] = json!({"plural": "gizmos", "kind": "Widget"});
    c.send_json(Method::POST, crds, &other).await.unwrap();
    let got = eventually("NamesAccepted=False", T, || async {
        let o = c.get_json(&format!("{crds}/gizmos.example.com")).await.ok()?;
        let cond = o["status"]["conditions"].as_array()?.iter().find(|x| x["type"] == "NamesAccepted")?.clone();
        (cond["status"] == "False").then_some(cond)
    })
    .await;
    // Like upstream, the last conflicting name wins (the defaulted listKind `WidgetList`).
    assert_eq!(got["reason"], "ListKindConflict");
    assert_eq!(got["message"], r#""WidgetList" is already in use"#);
    assert!(c.get_json("/apis/example.com/v1/gizmos").await.unwrap_err().is_not_found());
}
