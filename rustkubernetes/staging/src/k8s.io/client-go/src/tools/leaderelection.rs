//! Port of `client-go/tools/leaderelection` with a `Lease` lock.
//!
//! Only one replica of a component (kube-controller-manager, kube-scheduler)
//! does work at a time. Candidates compete for a `coordination.k8s.io/v1
//! Lease`: the holder renews `spec.renewTime` every `retry_period`; others
//! take over once `renewTime + leaseDurationSeconds` has passed.

use std::future::Future;
use std::time::Duration;

use chrono::{DateTime, Utc};
use k8s_api::coordination::v1::{Lease, LeaseSpec};
use k8s_apimachinery::meta::ObjectMeta;

use crate::api::Api;
use crate::rest::{Client, Error};

#[derive(Clone, Debug)]
pub struct LeaderElectionConfig {
    pub namespace: String,
    pub name: String,
    /// Unique identity of this candidate (hostname + random suffix).
    pub identity: String,
    pub lease_duration: Duration,
    pub renew_deadline: Duration,
    pub retry_period: Duration,
}

impl LeaderElectionConfig {
    /// Defaults of the Kubernetes components (15s / 10s / 2s).
    pub fn new(namespace: &str, name: &str, identity: &str) -> Self {
        Self {
            namespace: namespace.into(),
            name: name.into(),
            identity: identity.into(),
            lease_duration: Duration::from_secs(15),
            renew_deadline: Duration::from_secs(10),
            retry_period: Duration::from_secs(2),
        }
    }
}

/// `MicroTime` formatting.
fn micro(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

fn parse(s: &Option<String>) -> Option<DateTime<Utc>> {
    s.as_deref().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|t| t.with_timezone(&Utc))
}

pub struct LeaderElector {
    cfg: LeaderElectionConfig,
    api: Api<Lease>,
}

impl LeaderElector {
    pub fn new(client: Client, cfg: LeaderElectionConfig) -> Self {
        let api = Api::namespaced(client, &cfg.namespace);
        Self { cfg, api }
    }

    /// One acquire-or-renew attempt. Returns true if we hold the lease afterwards.
    pub async fn try_acquire_or_renew(&self) -> Result<bool, Error> {
        let now = Utc::now();
        let duration = self.cfg.lease_duration.as_secs() as i32;
        let Some(mut lease) = self.api.get_opt(&self.cfg.name).await? else {
            let mut lease = Lease::new(ObjectMeta::namespaced(&self.cfg.namespace, &self.cfg.name));
            lease.spec = Some(LeaseSpec {
                holder_identity: Some(self.cfg.identity.clone()),
                lease_duration_seconds: Some(duration),
                acquire_time: Some(micro(now)),
                renew_time: Some(micro(now)),
                lease_transitions: Some(0),
            });
            return match self.api.create(&lease).await {
                Ok(_) => Ok(true),
                Err(e) if e.is_already_exists() => Ok(false),
                Err(e) => Err(e),
            };
        };
        let spec = lease.spec.clone().unwrap_or_default();
        let holder = spec.holder_identity.clone().unwrap_or_default();
        let renewed = parse(&spec.renew_time).unwrap_or(DateTime::<Utc>::MIN_UTC);
        let ttl = chrono::Duration::seconds(spec.lease_duration_seconds.unwrap_or(duration) as i64);
        let i_hold = holder == self.cfg.identity;
        if !i_hold && !holder.is_empty() && renewed + ttl > now {
            return Ok(false); // someone else holds a valid lease
        }
        let mut new_spec = spec.clone();
        if !i_hold {
            new_spec.acquire_time = Some(micro(now));
            new_spec.lease_transitions = Some(spec.lease_transitions.unwrap_or(0) + 1);
        }
        new_spec.holder_identity = Some(self.cfg.identity.clone());
        new_spec.lease_duration_seconds = Some(duration);
        new_spec.renew_time = Some(micro(now));
        lease.spec = Some(new_spec);
        match self.api.replace(&lease).await {
            Ok(_) => Ok(true),
            Err(e) if e.is_conflict() => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Blocks until this candidate becomes leader, then runs `on_started` while
    /// renewing. Returns (with an error) when leadership is lost, like
    /// `OnStoppedLeading` — callers should exit the process.
    pub async fn run<F: Future<Output = ()> + Send + 'static>(self, on_started: F) -> Result<(), String> {
        tracing::info!(lease = %format!("{}/{}", self.cfg.namespace, self.cfg.name), identity = %self.cfg.identity, "attempting to acquire leader lease");
        loop {
            match self.try_acquire_or_renew().await {
                Ok(true) => break,
                Ok(false) => {}
                Err(e) => tracing::warn!("error acquiring lease: {e}"),
            }
            tokio::time::sleep(self.cfg.retry_period).await;
        }
        tracing::info!(identity = %self.cfg.identity, "successfully acquired lease");
        let work = tokio::spawn(on_started);
        let mut last_renew = tokio::time::Instant::now();
        loop {
            tokio::time::sleep(self.cfg.retry_period).await;
            match self.try_acquire_or_renew().await {
                Ok(true) => last_renew = tokio::time::Instant::now(),
                Ok(false) => {
                    work.abort();
                    return Err("leader lease taken by another candidate".into());
                }
                Err(e) => {
                    tracing::warn!("failed to renew lease: {e}");
                    if last_renew.elapsed() > self.cfg.renew_deadline {
                        work.abort();
                        return Err(format!("failed to renew lease within {:?}", self.cfg.renew_deadline));
                    }
                }
            }
        }
    }

    /// Gives up the lease (sets an empty holder) so another candidate can take over immediately.
    pub async fn release(&self) {
        if let Ok(Some(mut lease)) = self.api.get_opt(&self.cfg.name).await {
            if let Some(spec) = lease.spec.as_mut() {
                if spec.holder_identity.as_deref() == Some(&self.cfg.identity) {
                    spec.holder_identity = None;
                    spec.lease_duration_seconds = Some(1);
                    let _ = self.api.replace(&lease).await;
                }
            }
        }
    }
}
