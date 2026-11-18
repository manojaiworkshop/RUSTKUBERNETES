//! Port of `k8s.io/apimachinery/pkg/api/errors` and `metav1.Status`.

use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;

use crate::meta::ListMeta;
use crate::validation::FieldError;

/// `metav1.Status`: returned for errors and some delete calls.
#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub kind: Option<String>,
    pub api_version: Option<String>,
    #[serde(default)]
    pub metadata: ListMeta,
    /// `Success` or `Failure`
    pub status: Option<String>,
    pub message: Option<String>,
    pub reason: Option<String>,
    pub details: Option<StatusDetails>,
    pub code: Option<u16>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusDetails {
    pub name: Option<String>,
    pub group: Option<String>,
    pub kind: Option<String>,
    pub uid: Option<String>,
    pub causes: Option<Vec<StatusCause>>,
    pub retry_after_seconds: Option<i64>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusCause {
    pub reason: Option<String>,
    pub message: Option<String>,
    pub field: Option<String>,
}

/// Well-known `StatusReason` values.
pub mod reason {
    pub const NOT_FOUND: &str = "NotFound";
    pub const ALREADY_EXISTS: &str = "AlreadyExists";
    pub const CONFLICT: &str = "Conflict";
    pub const INVALID: &str = "Invalid";
    pub const BAD_REQUEST: &str = "BadRequest";
    pub const METHOD_NOT_ALLOWED: &str = "MethodNotAllowed";
    pub const NOT_ACCEPTABLE: &str = "NotAcceptable";
    pub const UNSUPPORTED_MEDIA_TYPE: &str = "UnsupportedMediaType";
    pub const GONE: &str = "Expired";
    pub const FORBIDDEN: &str = "Forbidden";
    pub const INTERNAL_ERROR: &str = "InternalError";
    pub const TIMEOUT: &str = "Timeout";
}

/// An API error. Converts 1:1 into a [`Status`] response body.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct ApiError {
    pub code: u16,
    pub reason: String,
    pub message: String,
    pub details: Option<StatusDetails>,
}

impl ApiError {
    pub fn new(code: u16, reason: &str, message: impl Into<String>) -> Self {
        Self { code, reason: reason.to_string(), message: message.into(), details: None }
    }

    pub fn with_details(mut self, group: &str, kind_or_resource: &str, name: &str) -> Self {
        self.details = Some(StatusDetails {
            name: Some(name.to_string()).filter(|s| !s.is_empty()),
            group: Some(group.to_string()).filter(|s| !s.is_empty()),
            kind: Some(kind_or_resource.to_string()),
            ..Default::default()
        });
        self
    }

    /// `pods "foo" not found`
    pub fn not_found(group: &str, resource: &str, name: &str) -> Self {
        Self::new(404, reason::NOT_FOUND, format!("{} \"{}\" not found", qualified(group, resource), name))
            .with_details(group, resource, name)
    }

    /// `pods "foo" already exists`
    pub fn already_exists(group: &str, resource: &str, name: &str) -> Self {
        Self::new(409, reason::ALREADY_EXISTS, format!("{} \"{}\" already exists", qualified(group, resource), name))
            .with_details(group, resource, name)
    }

    /// Optimistic-concurrency conflict.
    pub fn conflict(group: &str, resource: &str, name: &str, why: &str) -> Self {
        Self::new(409, reason::CONFLICT, format!("Operation cannot be fulfilled on {} \"{}\": {}", qualified(group, resource), name, why))
            .with_details(group, resource, name)
    }

    pub fn conflict_modified(group: &str, resource: &str, name: &str) -> Self {
        Self::conflict(group, resource, name, "the object has been modified; please apply your changes to the latest version and try again")
    }

    /// `Pod "foo" is invalid: spec.containers: Required value`
    pub fn invalid(group: &str, kind: &str, name: &str, errs: &[FieldError]) -> Self {
        let summary = errs.iter().map(|e| e.to_string()).collect::<Vec<_>>();
        let msg = if summary.len() == 1 {
            format!("{kind} \"{name}\" is invalid: {}", summary[0])
        } else {
            format!("{kind} \"{name}\" is invalid: [{}]", summary.join(", "))
        };
        let mut e = Self::new(422, reason::INVALID, msg).with_details(group, kind, name);
        if let Some(d) = e.details.as_mut() {
            d.causes = Some(
                errs.iter()
                    .map(|fe| StatusCause {
                        reason: Some(fe.kind.reason().to_string()),
                        message: Some(fe.message()),
                        field: Some(fe.field.clone()),
                    })
                    .collect(),
            );
        }
        e
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(400, reason::BAD_REQUEST, msg)
    }

    pub fn method_not_allowed(msg: impl Into<String>) -> Self {
        Self::new(405, reason::METHOD_NOT_ALLOWED, msg)
    }

    pub fn unsupported_media_type(ct: &str) -> Self {
        Self::new(415, reason::UNSUPPORTED_MEDIA_TYPE, format!("the body of the request was in an unknown format - accepted media types include: application/json, application/yaml, application/merge-patch+json, application/strategic-merge-patch+json, application/json-patch+json, application/apply-patch+yaml (got {ct:?})"))
    }

    pub fn gone(msg: impl Into<String>) -> Self {
        Self::new(410, reason::GONE, msg)
    }

    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::new(403, reason::FORBIDDEN, msg)
    }

    /// 422 for a single field problem in a non-stored request object (reviews, TokenRequest).
    pub fn invalid_simple(field: &str, msg: &str) -> Self {
        Self::new(422, "Invalid", format!("{field}: Invalid value: {msg}"))
    }

    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self::new(401, "Unauthorized", msg)
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(500, reason::INTERNAL_ERROR, format!("Internal error occurred: {}", msg.into()))
    }

    pub fn is_not_found(&self) -> bool {
        self.code == 404
    }

    pub fn is_already_exists(&self) -> bool {
        self.reason == reason::ALREADY_EXISTS
    }

    pub fn is_conflict(&self) -> bool {
        self.code == 409 && self.reason == reason::CONFLICT
    }

    pub fn to_status(&self) -> Status {
        Status {
            kind: Some("Status".into()),
            api_version: Some("v1".into()),
            metadata: ListMeta::default(),
            status: Some("Failure".into()),
            message: Some(self.message.clone()),
            reason: Some(self.reason.clone()),
            details: self.details.clone(),
            code: Some(self.code),
        }
    }

    pub fn from_status(s: &Status) -> Self {
        Self {
            code: s.code.unwrap_or(500),
            reason: s.reason.clone().unwrap_or_default(),
            message: s.message.clone().unwrap_or_default(),
            details: s.details.clone(),
        }
    }
}

impl Status {
    pub fn success() -> Self {
        Status {
            kind: Some("Status".into()),
            api_version: Some("v1".into()),
            status: Some("Success".into()),
            code: Some(200),
            ..Default::default()
        }
    }
}

/// `pods` for the core group, `deployments.apps` otherwise.
fn qualified(group: &str, resource: &str) -> String {
    if group.is_empty() {
        resource.to_string()
    } else {
        format!("{resource}.{group}")
    }
}
