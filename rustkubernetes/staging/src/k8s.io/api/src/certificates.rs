//! `certificates.k8s.io/v1`.

pub mod v1 {
    use std::collections::BTreeMap;

    use k8s_apimachinery::meta::ObjectMeta;
    use serde::{Deserialize, Serialize};
    use serde_with::skip_serializing_none;

    use crate::impl_resource;

    /// `KubeAPIServerClientSignerName`: client certificates the API server honours.
    pub const KUBE_APISERVER_CLIENT_SIGNER_NAME: &str = "kubernetes.io/kube-apiserver-client";
    /// `KubeAPIServerClientKubeletSignerName`: kubelet client certificates.
    pub const KUBE_APISERVER_CLIENT_KUBELET_SIGNER_NAME: &str = "kubernetes.io/kube-apiserver-client-kubelet";
    /// `KubeletServingSignerName`: kubelet serving certificates.
    pub const KUBELET_SERVING_SIGNER_NAME: &str = "kubernetes.io/kubelet-serving";
    /// `LegacyUnknownSignerName` (only valid in v1beta1).
    pub const LEGACY_UNKNOWN_SIGNER_NAME: &str = "kubernetes.io/legacy-unknown";

    pub const CERTIFICATE_APPROVED: &str = "Approved";
    pub const CERTIFICATE_DENIED: &str = "Denied";
    pub const CERTIFICATE_FAILED: &str = "Failed";

    /// `CertificateSigningRequest`: a request for a certificate signed by `spec.signerName`.
    #[skip_serializing_none]
    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct CertificateSigningRequest {
        pub api_version: Option<String>,
        pub kind: Option<String>,
        #[serde(default)]
        pub metadata: ObjectMeta,
        #[serde(default)]
        pub spec: CertificateSigningRequestSpec,
        pub status: Option<CertificateSigningRequestStatus>,
    }
    impl_resource!(
        CertificateSigningRequest,
        "certificates.k8s.io",
        "v1",
        "CertificateSigningRequest",
        "certificatesigningrequests",
        false
    );

    #[skip_serializing_none]
    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct CertificateSigningRequestSpec {
        /// PEM `CERTIFICATE REQUEST`, base64-encoded in JSON (Go `[]byte`).
        #[serde(default)]
        pub request: String,
        #[serde(default)]
        pub signer_name: String,
        pub expiration_seconds: Option<i32>,
        pub usages: Option<Vec<String>>,
        /// Set by the API server from the requesting user.
        pub username: Option<String>,
        pub uid: Option<String>,
        pub groups: Option<Vec<String>>,
        pub extra: Option<BTreeMap<String, Vec<String>>>,
    }

    #[skip_serializing_none]
    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct CertificateSigningRequestStatus {
        pub conditions: Option<Vec<CertificateSigningRequestCondition>>,
        /// PEM `CERTIFICATE` chain, base64-encoded in JSON.
        pub certificate: Option<String>,
    }

    #[skip_serializing_none]
    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct CertificateSigningRequestCondition {
        #[serde(rename = "type")]
        pub type_: String,
        #[serde(default)]
        pub status: String,
        pub reason: Option<String>,
        pub message: Option<String>,
        pub last_update_time: Option<String>,
        pub last_transition_time: Option<String>,
    }

    impl CertificateSigningRequest {
        pub fn conditions(&self) -> &[CertificateSigningRequestCondition] {
            self.status.as_ref().and_then(|s| s.conditions.as_deref()).unwrap_or_default()
        }

        /// `GetCertApprovalCondition`: (approved, denied).
        pub fn approval(&self) -> (bool, bool) {
            let c = self.conditions();
            (c.iter().any(|c| c.type_ == CERTIFICATE_APPROVED), c.iter().any(|c| c.type_ == CERTIFICATE_DENIED))
        }

        /// `IsCertificateRequestApproved`
        pub fn is_approved(&self) -> bool {
            let (a, d) = self.approval();
            a && !d
        }

        /// `HasTrueCondition`: status `True` or empty.
        pub fn has_true_condition(&self, t: &str) -> bool {
            self.conditions().iter().any(|c| c.type_ == t && (c.status.is_empty() || c.status == "True"))
        }

        /// The issued certificate (base64 in JSON), if any.
        pub fn certificate(&self) -> Option<&str> {
            self.status.as_ref().and_then(|s| s.certificate.as_deref()).filter(|c| !c.is_empty())
        }
    }
}
