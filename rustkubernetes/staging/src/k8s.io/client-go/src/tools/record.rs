//! Port of `client-go/tools/record`: an `EventRecorder` that writes
//! `core/v1` Events (shown by `kubectl describe` and `kubectl get events`).
//!
//! Events are fire-and-forget: they are posted from a background task and
//! failures are only logged, exactly like in Go.

use chrono::Utc;
use k8s_api::core::v1::{Event, EventSource, ObjectReference};
use k8s_api::Resource;
use k8s_apimachinery::meta::ObjectMeta;

use crate::api::Api;
use crate::rest::Client;

pub const NORMAL: &str = "Normal";
pub const WARNING: &str = "Warning";

#[derive(Clone)]
pub struct EventRecorder {
    client: Client,
    component: String,
    host: Option<String>,
}

/// Builds an `ObjectReference` to a typed object.
pub fn object_ref<K: Resource>(obj: &K) -> ObjectReference {
    let m = obj.metadata();
    ObjectReference {
        kind: Some(K::KIND.into()),
        namespace: m.namespace.clone(),
        name: m.name.clone(),
        uid: m.uid.clone(),
        api_version: Some(K::api_version()),
        resource_version: m.resource_version.clone(),
        field_path: None,
    }
}

impl EventRecorder {
    pub fn new(client: Client, component: &str) -> Self {
        Self { client, component: component.into(), host: None }
    }

    pub fn with_host(mut self, host: &str) -> Self {
        self.host = Some(host.into());
        self
    }

    /// Records an event about `obj` (`recorder.Eventf`).
    pub fn event<K: Resource>(&self, obj: &K, type_: &str, reason: &str, message: impl Into<String>) {
        self.event_ref(object_ref(obj), type_, reason, message.into());
    }

    pub fn event_ref(&self, involved: ObjectReference, type_: &str, reason: &str, message: String) {
        let now = Utc::now();
        let ns = involved.namespace.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| "default".into());
        let name = format!("{}.{:x}", involved.name.clone().unwrap_or_default(), now.timestamp_nanos_opt().unwrap_or(0));
        let ts = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let mut ev = Event::new(ObjectMeta::namespaced(&ns, name));
        ev.involved_object = involved;
        ev.reason = Some(reason.into());
        ev.message = Some(message);
        ev.type_ = Some(type_.into());
        ev.source = Some(EventSource { component: Some(self.component.clone()), host: self.host.clone() });
        ev.first_timestamp = Some(ts.clone());
        ev.last_timestamp = Some(ts);
        ev.count = Some(1);
        ev.reporting_component = Some(self.component.clone());
        let api: Api<Event> = Api::namespaced(self.client.clone(), &ns);
        tracing::debug!(reason, "event");
        tokio::spawn(async move {
            if let Err(e) = api.create(&ev).await {
                tracing::debug!("failed to record event: {e}");
            }
        });
    }
}
