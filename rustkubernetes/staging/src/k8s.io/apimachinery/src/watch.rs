//! Port of `k8s.io/apimachinery/pkg/watch`.

use serde::{Deserialize, Serialize};

/// Watch event type as sent on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum EventType {
    Added,
    Modified,
    Deleted,
    Bookmark,
    Error,
}

impl EventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventType::Added => "ADDED",
            EventType::Modified => "MODIFIED",
            EventType::Deleted => "DELETED",
            EventType::Bookmark => "BOOKMARK",
            EventType::Error => "ERROR",
        }
    }
}

/// One line of a watch stream: `{"type":"ADDED","object":{...}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WatchEvent<T> {
    #[serde(rename = "type")]
    pub type_: EventType,
    pub object: T,
}
