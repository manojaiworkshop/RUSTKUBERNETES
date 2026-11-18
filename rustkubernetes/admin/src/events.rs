//! Live updates: named topics whose latest value is pushed to browsers over
//! Server-Sent Events (`/api/events?topics=a,b`), like pgrustadmin. A topic is
//! sent once when a client connects and again whenever it changes.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;
use tokio::sync::broadcast;

pub struct Hub {
    latest: Mutex<HashMap<String, Value>>,
    tx: broadcast::Sender<(String, Value)>,
}

impl Default for Hub {
    fn default() -> Self {
        Self { latest: Mutex::new(HashMap::new()), tx: broadcast::channel(1024).0 }
    }
}

impl Hub {
    /// Publishes `value` on `topic` if it differs from the last value.
    pub fn publish(&self, topic: &str, value: Value) {
        let mut l = self.latest.lock().unwrap();
        if l.get(topic) == Some(&value) {
            return;
        }
        l.insert(topic.to_string(), value.clone());
        let _ = self.tx.send((topic.to_string(), value));
    }

    pub fn latest(&self, topic: &str) -> Option<Value> {
        self.latest.lock().unwrap().get(topic).cloned()
    }

    pub fn forget(&self, topic: &str) {
        self.latest.lock().unwrap().remove(topic);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<(String, Value)> {
        self.tx.subscribe()
    }
}
