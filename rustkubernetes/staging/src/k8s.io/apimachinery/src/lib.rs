//! Rust port of `k8s.io/apimachinery`.
//!
//! | Module         | Go equivalent                                  |
//! |----------------|------------------------------------------------|
//! | [`meta`]       | `pkg/apis/meta/v1` (ObjectMeta, TypeMeta, ...)  |
//! | [`errors`]     | `pkg/api/errors` (StatusError)                 |
//! | [`labels`]     | `pkg/labels` (label selectors)                 |
//! | [`fields`]     | `pkg/fields` (field selectors)                 |
//! | [`schema`]     | `pkg/runtime/schema` (GVK / GVR)               |
//! | [`watch`]      | `pkg/watch` (watch events)                     |
//! | [`validation`] | `pkg/util/validation` (+ `field.ErrorList`)    |
//! | [`unstructured`]| `pkg/apis/meta/v1/unstructured` helpers       |
//! | [`quantity`]   | `pkg/api/resource` (Quantity parsing)          |
//! | [`net`]        | `k8s.io/utils/net` (CIDR parsing)              |

pub mod duration;
pub mod errors;
pub mod fields;
pub mod labels;
pub mod meta;
pub mod net;
pub mod quantity;
pub mod schema;
pub mod unstructured;
pub mod validation;
pub mod watch;

pub use errors::{ApiError, Status, StatusCause};
pub use meta::{ListMeta, ObjectMeta, OwnerReference, TypeMeta};
pub use schema::{GroupVersion, GroupVersionKind, GroupVersionResource};
