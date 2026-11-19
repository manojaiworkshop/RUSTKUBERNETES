//! Port of `pkg/controller/certificates/approver` (`sarapprove.go`):
//! `certificatesigningrequest-approving-controller`.
//!
//! Kubelet client requests (`kubernetes.io/kube-apiserver-client-kubelet`) are
//! approved when the requester may `create` the matching subresource:
//!
//! | Recognizer | Subresource | Granted by default to |
//! |------------|-------------|-----------------------|
//! | the node renewing its own certificate (`CN` = requester) | `certificatesigningrequests/selfnodeclient` | — (kubeadm binds `system:nodes`) |
//! | any node client certificate | `certificatesigningrequests/nodeclient` | — (kubeadm binds bootstrap tokens) |

use std::sync::Arc;

use k8s_api::certificates::v1::*;
use k8s_client_go::{Api, Client};
use kube_apis::certificates::{validate_kubelet_client_csr, ParsedCsr};
use serde_json::json;

use super::{condition, ignorable, parse_request, CertificateController};
use crate::ControllerContext;

struct Recognizer {
    recognize: fn(&CertificateSigningRequest, &ParsedCsr) -> bool,
    subresource: &'static str,
    success_message: &'static str,
}

fn is_node_client_cert(csr: &CertificateSigningRequest, req: &ParsedCsr) -> bool {
    csr.spec.signer_name == KUBE_APISERVER_CLIENT_KUBELET_SIGNER_NAME
        && validate_kubelet_client_csr(req, csr.spec.usages.as_deref().unwrap_or_default()).is_ok()
}

fn is_self_node_client_cert(csr: &CertificateSigningRequest, req: &ParsedCsr) -> bool {
    csr.spec.username.as_deref().unwrap_or_default() == req.common_name && is_node_client_cert(csr, req)
}

fn recognizers() -> [Recognizer; 2] {
    [
        Recognizer {
            recognize: is_self_node_client_cert,
            subresource: "selfnodeclient",
            success_message: "Auto approving self kubelet client certificate after SubjectAccessReview.",
        },
        Recognizer {
            recognize: is_node_client_cert,
            subresource: "nodeclient",
            success_message: "Auto approving kubelet client certificate after SubjectAccessReview.",
        },
    ]
}

struct SarApprover {
    client: Client,
}

impl SarApprover {
    /// `handle`
    async fn handle(&self, mut csr: CertificateSigningRequest) -> anyhow::Result<()> {
        if csr.certificate().is_some() {
            return Ok(());
        }
        let (approved, denied) = csr.approval();
        if approved || denied {
            return Ok(());
        }
        let req = parse_request(&csr)?;
        let mut tried = vec![];
        for r in recognizers() {
            if !(r.recognize)(&csr, &req) {
                continue;
            }
            tried.push(r.subresource);
            if self.authorize(&csr, r.subresource).await? {
                csr.status.get_or_insert_with(Default::default).conditions.get_or_insert_with(Vec::new).push(condition(
                    CERTIFICATE_APPROVED,
                    "AutoApproved",
                    r.success_message,
                ));
                let name = csr.metadata.name.clone().unwrap_or_default();
                let api: Api<CertificateSigningRequest> = Api::all(self.client.clone());
                api.dynamic()
                    .replace_sub(&name, "approval", &serde_json::to_value(&csr)?)
                    .await
                    .map_err(|e| anyhow::anyhow!("error updating approval for csr: {e}"))?;
                tracing::info!(csr = %name, "{}", r.success_message);
                return Ok(());
            }
        }
        if !tried.is_empty() {
            return Err(ignorable(format!(
                "recognized csr {:?} as [{}] but subject access review was not approved",
                csr.metadata.name.as_deref().unwrap_or_default(),
                tried.join(" ")
            )));
        }
        Ok(())
    }

    /// `authorize`: a SubjectAccessReview for the requester.
    async fn authorize(&self, csr: &CertificateSigningRequest, subresource: &str) -> anyhow::Result<bool> {
        let s = &csr.spec;
        let sar = json!({
            "apiVersion": "authorization.k8s.io/v1",
            "kind": "SubjectAccessReview",
            "spec": {
                "user": s.username, "uid": s.uid, "groups": s.groups, "extra": s.extra,
                "resourceAttributes": {"group": "certificates.k8s.io", "resource": "certificatesigningrequests",
                    "verb": "create", "subresource": subresource, "version": "*"},
            },
        });
        let out = self.client.send_json(reqwest::Method::POST, "/apis/authorization.k8s.io/v1/subjectaccessreviews", &sar).await?;
        Ok(out.pointer("/status/allowed").and_then(serde_json::Value::as_bool).unwrap_or(false))
    }
}

/// `NewCSRApprovingController`
pub fn new_controller(ctx: &ControllerContext) -> Arc<CertificateController> {
    let a = Arc::new(SarApprover { client: ctx.client.clone() });
    CertificateController::new(
        "csrapproving",
        ctx,
        Arc::new(move |csr| {
            let a = a.clone();
            Box::pin(async move { a.handle(csr).await })
        }),
    )
}
