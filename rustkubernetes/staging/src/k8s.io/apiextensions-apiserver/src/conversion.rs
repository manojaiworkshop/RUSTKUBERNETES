//! Port of `apiextensions-apiserver/pkg/apiserver/conversion/webhook_converter.go`:
//! the `Webhook` conversion strategy sends a `ConversionReview` to the CRD's
//! conversion webhook (`spec.conversion.webhook.clientConfig`: a URL, or a
//! Service reached through its ClusterIP, verified with `caBundle` against
//! `<service>.<namespace>.svc`).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use k8s_apiserver::storage::converting::Converter;
use k8s_apiserver::Storage;
use serde_json::{json, Value};

use crate::apis::str_at;

pub struct WebhookConverter {
    crd: Arc<Value>,
    storage: Arc<dyn Storage>,
}

impl WebhookConverter {
    pub fn new(crd: Arc<Value>, storage: Arc<dyn Storage>) -> Self {
        Self { crd, storage }
    }

    /// `apiextensions.k8s.io/v1` if the webhook accepts it, else `v1beta1`.
    fn review_version(&self) -> &'static str {
        let accepted: Vec<&str> = self
            .crd
            .pointer("/spec/conversion/webhook/conversionReviewVersions")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if accepted.contains(&"v1") || !accepted.contains(&"v1beta1") {
            "apiextensions.k8s.io/v1"
        } else {
            "apiextensions.k8s.io/v1beta1"
        }
    }

    /// Where to POST and the client to use.
    async fn endpoint(&self) -> Result<(reqwest::Client, String), String> {
        let cc = self.crd.pointer("/spec/conversion/webhook/clientConfig").cloned().unwrap_or(Value::Null);
        k8s_client_go::util::cert::install_crypto_provider();
        let ca = match str_at(&cc, "/caBundle") {
            "" => None,
            b => Some(base64::engine::general_purpose::STANDARD.decode(b).map_err(|e| format!("invalid caBundle: {e}"))?),
        };
        let tls = k8s_client_go::util::cert::TlsClientConfig {
            ca_data: ca,
            cert_data: None,
            key_data: None,
            insecure: false,
            cert_resolver: None,
        }
        .rustls()?;
        let b = reqwest::Client::builder().tls_backend_preconfigured(tls).timeout(Duration::from_secs(30));
        if let Some(url) = cc.get("url").and_then(Value::as_str) {
            return Ok((b.build().map_err(|e| e.to_string())?, url.to_string()));
        }
        let (ns, name) = (str_at(&cc, "/service/namespace"), str_at(&cc, "/service/name"));
        let port = cc.pointer("/service/port").and_then(Value::as_i64).unwrap_or(443);
        let path = str_at(&cc, "/service/path");
        let svc = self
            .storage
            .get(&format!("/registry/services/specs/{ns}/{name}"))
            .await
            .map_err(|_| format!("service \"{name}\" not found"))?;
        let ip = str_at(&svc, "/spec/clusterIP");
        if ip.is_empty() || ip == "None" {
            return Err(format!("invalid service \"{name}\" with no cluster IP"));
        }
        let addr: std::net::SocketAddr =
            format!("{}:{port}", if ip.contains(':') { format!("[{ip}]") } else { ip.to_string() }).parse().map_err(|e| format!("{e}"))?;
        let host = format!("{name}.{ns}.svc");
        let client = b.resolve(&host, addr).build().map_err(|e| e.to_string())?;
        Ok((client, format!("https://{host}:{port}{path}")))
    }
}

#[async_trait]
impl Converter for WebhookConverter {
    async fn convert(&self, obj: Value, api_version: &str) -> Result<Value, String> {
        let kind = str_at(&self.crd, "/spec/names/kind").to_string();
        let group = str_at(&self.crd, "/spec/group").to_string();
        let fail = |e: String| format!("conversion webhook for {group}/{kind} failed: {e}");
        let uid = k8s_apimachinery::meta::new_uid();
        let review_version = self.review_version();
        let review = json!({
            "apiVersion": review_version, "kind": "ConversionReview",
            "request": {"uid": uid, "desiredAPIVersion": api_version, "objects": [obj]},
        });
        let (client, url) = self.endpoint().await.map_err(&fail)?;
        let resp = client.post(&url).json(&review).send().await.map_err(|e| fail(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(fail(format!("conversion webhook returned HTTP {}", resp.status().as_u16())));
        }
        let body: Value = resp.json().await.map_err(|e| fail(format!("invalid response: {e}")))?;
        let r = &body["response"];
        if str_at(r, "/uid") != uid {
            return Err(fail(format!("expected response.uid={uid:?}, got {:?}", str_at(r, "/uid"))));
        }
        if str_at(r, "/result/status") != "Success" {
            return Err(fail(str_at(r, "/result/message").to_string()));
        }
        let mut converted = r.get("convertedObjects").and_then(Value::as_array).cloned().unwrap_or_default();
        if converted.len() != 1 {
            return Err(fail(format!("expected 1 converted objects, got {}", converted.len())));
        }
        let out = converted.remove(0);
        if str_at(&out, "/apiVersion") != api_version {
            return Err(fail(format!("invalid converted object: expected apiVersion {api_version:?}")));
        }
        Ok(out)
    }
}
