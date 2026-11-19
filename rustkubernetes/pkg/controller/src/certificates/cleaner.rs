//! Port of `pkg/controller/certificates/cleaner`: `certificatesigningrequest-cleaner-controller`.
//!
//! Every hour all requests are checked and deleted when they are
//! issued and approved more than 1h ago, denied or failed more than 1h ago,
//! pending (no conditions) for more than 24h, approved but unissued for more
//! than 24h, or when their certificate has expired.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use k8s_api::certificates::v1::*;
use k8s_apimachinery::meta::parse_time;
use k8s_client_go::informers::SharedInformerFactory;
use k8s_client_go::tools::cache::Store;
use k8s_client_go::{Api, Client};

use crate::ControllerContext;

const POLLING_INTERVAL: Duration = Duration::from_secs(3600);
const APPROVED_EXPIRATION: i64 = 3600;
const DENIED_EXPIRATION: i64 = 3600;
const PENDING_EXPIRATION: i64 = 24 * 3600;

pub struct Controller {
    client: Client,
    csrs: Store<CertificateSigningRequest>,
    informers: Arc<SharedInformerFactory>,
}

fn updated(c: &CertificateSigningRequestCondition) -> Option<&str> {
    c.last_update_time.as_deref()
}

fn older_than(t: Option<&str>, secs: i64, now: i64) -> bool {
    t.and_then(parse_time).is_some_and(|t| now - t.timestamp() > secs)
}

fn not_after(csr: &CertificateSigningRequest) -> Option<i64> {
    let pem = base64::engine::general_purpose::STANDARD.decode(csr.certificate()?).ok()?;
    kube_apis::certificates::certificate_not_after(&pem)
}

/// The cleaner's reason to delete `csr` at `now` (unix seconds), if any.
pub fn should_delete(csr: &CertificateSigningRequest, now: i64) -> Option<&'static str> {
    let conds = csr.conditions();
    let issued = csr.certificate().is_some();
    if issued && conds.iter().any(|c| c.type_ == CERTIFICATE_APPROVED && older_than(updated(c), APPROVED_EXPIRATION, now)) {
        return Some("Cleaning CSR as it is more than approvedExpiration duration old and approved.");
    }
    if conds.iter().any(|c| c.type_ == CERTIFICATE_DENIED && older_than(updated(c), DENIED_EXPIRATION, now)) {
        return Some("Cleaning CSR as it is more than deniedExpiration duration old and denied.");
    }
    if conds.iter().any(|c| c.type_ == CERTIFICATE_FAILED && older_than(updated(c), DENIED_EXPIRATION, now)) {
        return Some("Cleaning CSR as it is more than deniedExpiration duration old and failed.");
    }
    if conds.is_empty() && older_than(csr.metadata.creation_timestamp.as_deref(), PENDING_EXPIRATION, now) {
        return Some("Cleaning CSR as it is more than pendingExpiration duration old and unhandled.");
    }
    if issued && conds.iter().any(|c| c.type_ == CERTIFICATE_APPROVED) && not_after(csr).is_some_and(|na| now > na) {
        return Some("Cleaning CSR as the associated certificate is expired.");
    }
    if !issued && conds.iter().any(|c| c.type_ == CERTIFICATE_APPROVED && older_than(updated(c), PENDING_EXPIRATION, now)) {
        return Some("Cleaning CSR as it is approved but unissued for more than pendingExpiration duration.");
    }
    None
}

impl Controller {
    pub fn new(ctx: &ControllerContext) -> Arc<Self> {
        let inf = ctx.informers.informer::<CertificateSigningRequest>();
        Arc::new(Self { client: ctx.client.clone(), csrs: inf.store(), informers: ctx.informers.clone() })
    }

    pub async fn run(self: Arc<Self>, _workers: usize) {
        self.informers.wait_for_cache_sync().await;
        loop {
            self.clean().await;
            tokio::time::sleep(POLLING_INTERVAL).await;
        }
    }

    async fn clean(&self) {
        let now = chrono::Utc::now().timestamp();
        let api: Api<CertificateSigningRequest> = Api::all(self.client.clone());
        for csr in self.csrs.list() {
            if let Some(why) = should_delete(&csr, now) {
                let name = csr.metadata.name.as_deref().unwrap_or_default();
                tracing::info!(csr = name, "{why}");
                if let Err(e) = api.delete(name).await {
                    if !e.is_not_found() {
                        tracing::warn!(csr = name, "unable to delete CSR {name:?}: {e}");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs_ago: i64, now: i64) -> String {
        chrono::DateTime::from_timestamp(now - secs_ago, 0).unwrap().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    #[test]
    fn deadlines() {
        let now = chrono::Utc::now().timestamp();
        let mut c = CertificateSigningRequest::default();
        c.metadata.creation_timestamp = Some(at(25 * 3600, now));
        assert!(should_delete(&c, now).unwrap().contains("pendingExpiration duration old and unhandled"));
        c.metadata.creation_timestamp = Some(at(60, now));
        assert!(should_delete(&c, now).is_none());
        let cond = |t: &str, ago: i64| CertificateSigningRequestCondition {
            type_: t.into(),
            status: "True".into(),
            last_update_time: Some(at(ago, now)),
            ..Default::default()
        };
        c.status = Some(CertificateSigningRequestStatus { conditions: Some(vec![cond(CERTIFICATE_DENIED, 7200)]), certificate: None });
        assert!(should_delete(&c, now).unwrap().contains("denied"));
        c.status = Some(CertificateSigningRequestStatus { conditions: Some(vec![cond(CERTIFICATE_APPROVED, 7200)]), certificate: None });
        assert!(should_delete(&c, now).is_none(), "approved, unissued, under 24h");
        c.status.as_mut().unwrap().certificate = Some("eA==".into());
        assert!(should_delete(&c, now).unwrap().contains("approvedExpiration"));
    }
}
