//! Rust port of `k8s.io/apiserver`: the generic API server library.
//!
//! | Module        | Go equivalent                                        |
//! |---------------|------------------------------------------------------|
//! | [`storage`]   | `pkg/storage` (+ `etcd3`, `cacher`): key/value store with watch |
//! | [`registry`]  | `pkg/registry/rest` + `generic/registry`: resource metadata and strategies |
//! | [`admission`] | `pkg/admission`: admission plugin chain              |
//! | [`authentication`] | `pkg/authentication`: x509, token file, anonymous |
//! | [`authorization`] | `pkg/authorization`: attributes, authorizer chain |
//! | [`impersonation`] | `endpoints/filters/impersonation.go` |
//! | [`patch`]     | JSON patch, merge patch, strategic merge patch        |
//! | [`table`]     | server-side `Table` printing (`kubectl get` columns) |
//! | [`request`]   | `endpoints/request`: URL → `RequestInfo`             |
//! | [`handlers`]  | `endpoints/handlers`: get/list/create/update/patch/delete/watch |
//! | [`openapi`]   | `kube-openapi`: `/openapi/v3` documents          |
//! | [`discovery`] | `endpoints/discovery`: `/api`, `/apis`, resource lists |
//! | [`proxy`]     | `pods/log` streaming + `pods/exec` WebSocket proxy to the kubelet |
//! | [`warning`]   | `pkg/warning`: `Warning` response headers |
//! | [`server`]    | `pkg/server`: `GenericAPIServer`, routing, health    |

pub mod admission;
pub mod aggregator;
pub mod authentication;
pub mod authorization;
pub mod discovery;
pub mod handlers;
pub mod impersonation;
pub mod openapi;
pub mod patch;
pub mod proxy;
pub mod registry;
pub mod request;
pub mod server;
pub mod storage;
pub mod table;
pub mod warning;

pub use registry::{Registry, ResourceInfo, Strategy, StrategyContext};
pub use server::{ApiServer, ApiServerConfig};
pub use storage::{memory::MemoryStore, Storage};
