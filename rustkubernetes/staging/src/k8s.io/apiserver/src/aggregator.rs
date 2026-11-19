//! The hook through which the aggregation layer (`kube-aggregator`) sits in
//! front of this server: `/apis` gains the groups of extension API servers, and
//! requests for a group/version that an `APIService` assigns to a Service are
//! proxied there instead of being handled locally.

use async_trait::async_trait;
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use bytes::Bytes;
use k8s_apimachinery::errors::ApiError;
use serde_json::Value;

use crate::authentication::UserInfo;

/// An authenticated, authorized request that may belong to an extension API server.
pub struct AggregatedRequest<'a> {
    pub method: &'a Method,
    pub path: &'a str,
    pub raw_query: Option<&'a str>,
    pub headers: &'a HeaderMap,
    pub body: Bytes,
    pub user: &'a UserInfo,
}

#[async_trait]
pub trait Aggregator: Send + Sync {
    /// `APIGroup` documents of groups served by extension API servers, in priority order.
    fn api_groups(&self) -> Vec<Value>;

    /// Serves `/apis/<group>[/<version>/...]` when an extension API server owns it;
    /// `None` leaves the request to the local handlers.
    async fn serve(&self, req: AggregatedRequest<'_>) -> Option<Result<Response, ApiError>>;
}
