//! Rust port of `k8s.io/client-go`.
//!
//! | Module                  | Go equivalent                              |
//! |-------------------------|--------------------------------------------|
//! | [`rest`]                | `rest` (Config, RESTClient)                |
//! | [`api`]                 | typed clientsets (`CoreV1().Pods(ns)`) + `dynamic` |
//! | [`tools::clientcmd`]    | `tools/clientcmd` (kubeconfig loading)     |
//! | [`tools::cache`]        | `tools/cache` (Reflector, Store, Informer) |
//! | [`tools::leaderelection`] | `tools/leaderelection` (Lease locks)     |
//! | [`tools::record`]       | `tools/record` (Event recorder)            |
//! | [`informers`]           | `informers` (SharedInformerFactory)        |
//! | [`discovery`]           | `discovery` (server resources)             |
//! | [`util::workqueue`]     | `util/workqueue` (rate-limited queue)      |
//!
//! ```no_run
//! # async fn demo() -> Result<(), k8s_client_go::Error> {
//! use k8s_api::core::v1::Pod;
//! use k8s_client_go::{Api, Client, ListParams};
//! let client = Client::try_default()?;
//! let pods: Api<Pod> = Api::namespaced(client, "default");
//! for p in pods.list(&ListParams::default()).await?.items {
//!     println!("{}", p.metadata.name());
//! }
//! # Ok(()) }
//! ```

pub mod api;
pub mod discovery;
pub mod informers;
pub mod rest;
pub mod tools;
pub mod util;

pub use api::{Api, DeleteParams, DynamicApi, ListParams, PatchType};
pub use rest::{Client, Config, Error};
