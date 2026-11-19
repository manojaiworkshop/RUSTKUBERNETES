//! Port of `k8s.io/apiextensions-apiserver`: `CustomResourceDefinition`s
//! (`apiextensions.k8s.io/v1`) and serving their custom resources.
//!
//! Upstream runs it as a delegate server behind kube-apiserver; here the CRD
//! resource is registered with the main [`k8s_apiserver::Registry`] and every
//! established CRD adds one runtime resource per served version, so the
//! generic handlers, discovery, watch, admission and authorization apply to
//! custom resources unchanged.
//!
//! | Module | Upstream |
//! |--------|----------|
//! | [`apis`] | `apiextensions-api/pkg/apis/apiextensions/v1` (defaults), `pkg/apihelpers` |
//! | [`validation`] | `pkg/apis/apiextensions/validation`, `pkg/apiserver/schema/validation.go` |
//! | [`schema`] | `pkg/apiserver/schema/{pruning,defaulting,objectmeta,listtype}`, kube-openapi `validate` |
//! | [`jsonpath`] | `client-go/util/jsonpath` (printer columns) |
//! | [`registry`] | `pkg/registry/customresourcedefinition`, `pkg/registry/customresource` |
//! | [`conversion`] | `pkg/apiserver/conversion` (`None`, `Webhook`) |
//! | [`controller`] | naming, establishing, finalizer, apiapproval, crdregistration controllers |

pub mod apis;
pub mod controller;
pub mod conversion;
pub mod jsonpath;
pub mod registry;
pub mod schema;
pub mod validation;

use std::sync::Arc;

use k8s_apiserver::registry::ResourceBuilder;
use k8s_apiserver::{ApiServer, Registry};

/// Registers `customresourcedefinitions.apiextensions.k8s.io`.
pub fn install(r: &mut Registry) {
    r.register(
        ResourceBuilder::new(apis::GROUP, apis::VERSION, "CustomResourceDefinition", "customresourcedefinitions", false)
            .short_names(&["crd", "crds"])
            .categories(&["api-extensions"])
            .subresources(&["status"])
            .strategy(Arc::new(registry::CrdStrategy))
            .build(),
    );
}

/// Starts serving established CRDs (before the server reports ready).
pub async fn start(server: Arc<ApiServer>) -> Arc<controller::CrdController> {
    controller::CrdController::start(server).await
}
