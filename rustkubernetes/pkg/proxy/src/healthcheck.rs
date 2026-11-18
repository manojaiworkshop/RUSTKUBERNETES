//! Proxier health and metrics (`pkg/proxy/healthcheck/proxier_health.go`, `metrics`).
//!
//! `/healthz` and `/livez` are healthy once rules were synced and no requested
//! update has been pending longer than the health timeout (2 × sync period).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use serde_json::json;

#[derive(Default)]
struct State {
    last_queued: Option<Instant>,
    last_updated: Option<Instant>,
    last_updated_wall: Option<String>,
    syncs: u64,
    errors: u64,
    sync_seconds_sum: f64,
    services: usize,
    endpoints: usize,
    rules_bytes: usize,
}

pub struct ProxierHealth {
    state: Mutex<State>,
    timeout: Duration,
}

impl ProxierHealth {
    pub fn new(timeout: Duration) -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(State::default()), timeout })
    }

    /// A change arrived that needs a sync (`QueuedUpdate`).
    pub fn queued(&self) {
        let mut s = self.state.lock().unwrap();
        if s.last_queued.is_none() {
            s.last_queued = Some(Instant::now());
        }
    }

    /// A sync finished.
    pub fn synced(&self, ok: bool, took: Duration, services: usize, endpoints: usize, rules_bytes: usize) {
        let mut s = self.state.lock().unwrap();
        s.syncs += 1;
        s.sync_seconds_sum += took.as_secs_f64();
        if ok {
            s.last_updated = Some(Instant::now());
            s.last_updated_wall = Some(chrono_now());
            s.last_queued = None;
            s.services = services;
            s.endpoints = endpoints;
            s.rules_bytes = rules_bytes;
        } else {
            s.errors += 1;
        }
    }

    pub fn healthy(&self) -> bool {
        let s = self.state.lock().unwrap();
        s.last_updated.is_some() && s.last_queued.is_none_or(|q| q.elapsed() < self.timeout)
    }

    fn metrics(&self) -> String {
        let s = self.state.lock().unwrap();
        format!(
            "# TYPE kubeproxy_sync_proxy_rules_duration_seconds summary\n\
             kubeproxy_sync_proxy_rules_duration_seconds_sum {}\n\
             kubeproxy_sync_proxy_rules_duration_seconds_count {}\n\
             # TYPE kubeproxy_sync_proxy_rules_errors_total counter\n\
             kubeproxy_sync_proxy_rules_errors_total {}\n\
             # TYPE kubeproxy_sync_proxy_rules_service_ports gauge\n\
             kubeproxy_sync_proxy_rules_service_ports {}\n\
             # TYPE kubeproxy_sync_proxy_rules_endpoints gauge\n\
             kubeproxy_sync_proxy_rules_endpoints {}\n\
             # TYPE kubeproxy_sync_proxy_rules_bytes gauge\n\
             kubeproxy_sync_proxy_rules_bytes {}\n",
            s.sync_seconds_sum, s.syncs, s.errors, s.services, s.endpoints, s.rules_bytes
        )
    }

    pub fn router(self: &Arc<Self>) -> Router {
        let h = self.clone();
        let m = self.clone();
        let status = move || {
            let h = h.clone();
            async move {
                let last = h.state.lock().unwrap().last_updated_wall.clone();
                let body = json!({"lastUpdated": last, "currentTime": chrono_now(), "nodeEligible": true}).to_string();
                let code = if h.healthy() { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
                (code, [("Content-Type", "application/json")], body).into_response()
            }
        };
        Router::new()
            .route("/healthz", get(status.clone()))
            .route("/livez", get(status))
            .route("/metrics", get(move || async move { m.metrics() }))
    }
}

fn chrono_now() -> String {
    k8s_apimachinery::meta::now_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_transitions() {
        let h = ProxierHealth::new(Duration::from_millis(50));
        assert!(!h.healthy()); // never synced
        h.synced(true, Duration::from_millis(3), 1, 2, 100);
        assert!(h.healthy());
        h.queued();
        assert!(h.healthy()); // pending, but within the timeout
        std::thread::sleep(Duration::from_millis(60));
        assert!(!h.healthy()); // update stuck
        h.synced(true, Duration::from_millis(3), 1, 2, 100);
        assert!(h.healthy());
        assert!(h.metrics().contains("kubeproxy_sync_proxy_rules_duration_seconds_count 2"));
    }
}
