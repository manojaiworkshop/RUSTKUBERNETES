//! Port of `k8s.io/kubernetes/pkg/registry`.
//!
//! One [`Strategy`](k8s_apiserver::Strategy) per built-in resource:
//!
//! | Module                 | Resources                                          |
//! |------------------------|----------------------------------------------------|
//! | [`core::pod`]          | pods (+ `binding`, graceful deletion, status)      |
//! | [`core::service`]      | services (+ ClusterIP / NodePort allocation)       |
//! | [`core::namespace`]    | namespaces (finalizer-based termination)           |
//! | [`batch`]              | jobs (+ generated selector), cronjobs, endpointslices |
//! | [`core::node`]         | nodes                                              |
//! | [`core::simple`]       | configmaps, secrets, serviceaccounts, endpoints, events |
//! | [`apps`]               | deployments, replicasets, daemonsets, statefulsets |
//! | [`generic`]            | table-driven strategy for simple resources         |

pub mod apps;
pub mod authentication;
pub mod authorization;
pub mod batch;
pub mod core;
pub mod generic;
pub mod rbac;
