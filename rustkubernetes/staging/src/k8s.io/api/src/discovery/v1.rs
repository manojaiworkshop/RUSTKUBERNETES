//! Port of `k8s.io/api/discovery/v1` (EndpointSlice).

use std::collections::BTreeMap;

use k8s_apimachinery::meta::ObjectMeta;
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;

use crate::core::v1::ObjectReference;
use crate::impl_resource;

/// Label linking a slice to its Service.
pub const LABEL_SERVICE_NAME: &str = "kubernetes.io/service-name";
/// Label naming the controller that manages a slice.
pub const LABEL_MANAGED_BY: &str = "endpointslice.kubernetes.io/managed-by";

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointSlice {
    pub api_version: Option<String>,
    pub kind: Option<String>,
    #[serde(default)]
    pub metadata: ObjectMeta,
    /// `IPv4`, `IPv6`, `FQDN`
    #[serde(default)]
    pub address_type: String,
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
    pub ports: Option<Vec<EndpointPort>>,
}
impl_resource!(EndpointSlice, "discovery.k8s.io", "v1", "EndpointSlice", "endpointslices", true);

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
    pub addresses: Vec<String>,
    pub conditions: Option<EndpointConditions>,
    pub hostname: Option<String>,
    pub target_ref: Option<ObjectReference>,
    pub node_name: Option<String>,
    pub zone: Option<String>,
    pub deprecated_topology: Option<BTreeMap<String, String>>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointConditions {
    pub ready: Option<bool>,
    pub serving: Option<bool>,
    pub terminating: Option<bool>,
}

#[skip_serializing_none]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointPort {
    pub name: Option<String>,
    pub protocol: Option<String>,
    pub port: Option<i32>,
    pub app_protocol: Option<String>,
}
