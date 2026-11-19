//! `plugin/pkg/admission/certificates`: who may approve and sign requests for a
//! signer, and the `system:masters` restriction on the client signer.
//!
//! | Plugin | Operation | Rule |
//! |--------|-----------|------|
//! | `CertificateApproval` | update `approval` | `approve` on `signers` (`certificates.k8s.io`) named the signer, or `<domain>/*` |
//! | `CertificateSigning` | update `status` (certificate or conditions changed) | `sign` on `signers` |
//! | `CertificateSubjectRestriction` | create | no `O=system:masters` for `kubernetes.io/kube-apiserver-client` |

use async_trait::async_trait;
use base64::Engine;
use k8s_apimachinery::errors::ApiError;
use k8s_apiserver::admission::{AdmissionPlugin, Attributes, Operation};
use k8s_apiserver::authentication::current_user;
use k8s_apiserver::authorization::{Attributes as AuthzAttributes, Authorizer, Decision};
use k8s_apiserver::registry::StrategyContext;
use serde_json::Value;

pub const APPROVAL_PLUGIN_NAME: &str = "CertificateApproval";
pub const SIGNING_PLUGIN_NAME: &str = "CertificateSigning";
pub const SUBJECT_RESTRICTION_PLUGIN_NAME: &str = "CertificateSubjectRestriction";

fn is_csr(a: &Attributes<'_>) -> bool {
    a.resource.group == "certificates.k8s.io" && a.resource.plural == "certificatesigningrequests"
}

fn signer_of(obj: &Value) -> &str {
    obj.pointer("/spec/signerName").and_then(Value::as_str).unwrap_or("")
}

/// `admission.NewForbidden`
fn forbidden(a: &Attributes<'_>, msg: impl std::fmt::Display) -> ApiError {
    ApiError::forbidden(format!("{} {:?} is forbidden: {msg}", a.resource.group_resource(), a.name))
}

/// `certauthorization.IsAuthorizedForSignerName`: the exact signer name, then `<domain>/*`.
pub async fn is_authorized_for_signer_name(authz: &dyn Authorizer, verb: &str, signer: &str) -> bool {
    let user = current_user().unwrap_or_default();
    let attrs = |name: String| AuthzAttributes {
        user: user.clone(),
        verb: verb.into(),
        resource_request: true,
        api_group: "certificates.k8s.io".into(),
        api_version: "*".into(),
        resource: "signers".into(),
        name,
        ..Default::default()
    };
    if matches!(authz.authorize(&attrs(signer.to_string())).await, Decision::Allow(_)) {
        return true;
    }
    let domain = signer.split('/').next().unwrap_or("");
    matches!(authz.authorize(&attrs(format!("{domain}/*"))).await, Decision::Allow(_))
}

/// `CertificateApproval`
pub struct CertificateApproval;

#[async_trait]
impl AdmissionPlugin for CertificateApproval {
    fn name(&self) -> &str {
        APPROVAL_PLUGIN_NAME
    }

    async fn validate(&self, a: &Attributes<'_>, _obj: &Value, ctx: &StrategyContext<'_>) -> Result<(), ApiError> {
        if a.operation != Operation::Update || a.subresource != Some("approval") || !is_csr(a) {
            return Ok(());
        }
        let signer = a.old_object.map(signer_of).unwrap_or("");
        if !is_authorized_for_signer_name(ctx.authorizer, "approve", signer).await {
            return Err(forbidden(a, format!("user not permitted to approve requests with signerName {signer:?}")));
        }
        Ok(())
    }
}

/// `CertificateSigning`
pub struct CertificateSigning;

#[async_trait]
impl AdmissionPlugin for CertificateSigning {
    fn name(&self) -> &str {
        SIGNING_PLUGIN_NAME
    }

    async fn validate(&self, a: &Attributes<'_>, obj: &Value, ctx: &StrategyContext<'_>) -> Result<(), ApiError> {
        if a.operation != Operation::Update || a.subresource != Some("status") || !is_csr(a) {
            return Ok(());
        }
        let Some(old) = a.old_object else { return Ok(()) };
        let unchanged = |p: &str| old.pointer(p) == obj.pointer(p);
        if unchanged("/status/certificate") && unchanged("/status/conditions") {
            return Ok(());
        }
        let signer = signer_of(old);
        if !is_authorized_for_signer_name(ctx.authorizer, "sign", signer).await {
            return Err(forbidden(a, format!("user not permitted to sign requests with signerName {signer:?}")));
        }
        Ok(())
    }
}

/// `CertificateSubjectRestriction`
pub struct CertificateSubjectRestriction;

#[async_trait]
impl AdmissionPlugin for CertificateSubjectRestriction {
    fn name(&self) -> &str {
        SUBJECT_RESTRICTION_PLUGIN_NAME
    }

    async fn validate(&self, a: &Attributes<'_>, obj: &Value, _ctx: &StrategyContext<'_>) -> Result<(), ApiError> {
        if a.operation != Operation::Create || a.subresource.is_some() || !is_csr(a) {
            return Ok(());
        }
        if signer_of(obj) != k8s_api::certificates::v1::KUBE_APISERVER_CLIENT_SIGNER_NAME {
            return Ok(());
        }
        let pem = obj
            .pointer("/spec/request")
            .and_then(Value::as_str)
            .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
            .unwrap_or_default();
        let csr = kube_apis::certificates::parse_csr(&pem).map_err(|e| forbidden(a, format!("failed to parse CSR: {e}")))?;
        if csr.organizations.iter().any(|o| o == "system:masters") {
            return Err(forbidden(
                a,
                format!(
                    "use of {} signer with system:masters group is not allowed",
                    k8s_api::certificates::v1::KUBE_APISERVER_CLIENT_SIGNER_NAME
                ),
            ));
        }
        Ok(())
    }
}
