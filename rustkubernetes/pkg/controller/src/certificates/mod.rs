//! Port of `pkg/controller/certificates`.
//!
//! | Controller | Does |
//! |------------|------|
//! | `certificatesigningrequest-signing-controller` ([`signer`]) | signs approved requests for the `kubernetes.io/*` signers with `--cluster-signing-*` CAs |
//! | `certificatesigningrequest-approving-controller` ([`approver`]) | approves kubelet client requests after a SubjectAccessReview (`nodeclient`, `selfnodeclient`) |
//! | `certificatesigningrequest-cleaner-controller` ([`cleaner`]) | deletes old requests |
//!
//! [`CertificateController`] is the shared loop (`certificate_controller.go`):
//! requests that already have a certificate are skipped, everything else goes
//! to the handler.

pub mod approver;
pub mod authority;
pub mod cleaner;
pub mod signer;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use k8s_api::certificates::v1::CertificateSigningRequest;
use k8s_client_go::tools::cache::Store;
use k8s_client_go::util::workqueue::WorkQueue;

use crate::controller_utils::*;
use crate::ControllerContext;

/// The `handler` of a [`CertificateController`]; [`ignorable`] errors are retried quietly.
pub type Handler = Arc<dyn Fn(CertificateSigningRequest) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> + Send + Sync>;

/// `IgnorableError`
pub fn ignorable(msg: impl Into<String>) -> anyhow::Error {
    Ignorable(msg.into()).into()
}

/// `CertificateController`
pub struct CertificateController {
    name: &'static str,
    csrs: Store<CertificateSigningRequest>,
    informers: Arc<k8s_client_go::informers::SharedInformerFactory>,
    queue: Queue,
    handler: Handler,
}

impl CertificateController {
    pub fn new(name: &'static str, ctx: &ControllerContext, handler: Handler) -> Arc<Self> {
        let inf = ctx.informers.informer::<CertificateSigningRequest>();
        let c = Arc::new(Self { name, csrs: inf.store(), informers: ctx.informers.clone(), queue: WorkQueue::new(), handler });
        let me = c.clone();
        on_events(inf.subscribe(), move |ev| me.queue.add(key(ev.object().as_ref())));
        c
    }

    pub async fn run(self: Arc<Self>, workers: usize) {
        self.informers.wait_for_cache_sync().await;
        let me = self.clone();
        run_workers(self.name, self.queue.clone(), workers, move |k| {
            let me = me.clone();
            async move { me.sync(&k).await }
        })
        .await;
    }

    /// `syncFunc`
    async fn sync(&self, k: &str) -> SyncResult {
        let Some(csr) = self.csrs.get(k) else { return Ok(None) };
        if csr.certificate().is_some() {
            return Ok(None);
        }
        (self.handler)((*csr).clone()).await?;
        Ok(None)
    }
}

/// `CertificateSigningRequestCondition` as JSON, `lastUpdateTime` now.
pub fn condition(type_: &str, reason: &str, message: &str) -> k8s_api::certificates::v1::CertificateSigningRequestCondition {
    k8s_api::certificates::v1::CertificateSigningRequestCondition {
        type_: type_.into(),
        status: "True".into(),
        reason: Some(reason.into()),
        message: Some(message.into()),
        last_update_time: Some(k8s_apimachinery::meta::now_rfc3339()),
        last_transition_time: None,
    }
}

/// Decodes `spec.request` (base64 in JSON) and parses it.
pub fn parse_request(csr: &CertificateSigningRequest) -> anyhow::Result<kube_apis::certificates::ParsedCsr> {
    use base64::Engine;
    let pem = base64::engine::general_purpose::STANDARD.decode(&csr.spec.request).unwrap_or_default();
    kube_apis::certificates::parse_csr(&pem)
        .map_err(|e| anyhow::anyhow!("unable to parse csr {:?}: {e}", csr.metadata.name.as_deref().unwrap_or_default()))
}
