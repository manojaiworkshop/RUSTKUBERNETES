//! Port of `k8s.io/apiserver/pkg/admission`.
//!
//! Admission plugins run after authentication/authorization and before the
//! object is validated and persisted. A plugin may mutate the object
//! ([`AdmissionPlugin::admit`]) and/or reject it ([`AdmissionPlugin::validate`]).
//! The chain runs every mutating step first, then every validating step.

use std::sync::Arc;

use async_trait::async_trait;
use k8s_apimachinery::errors::ApiError;
use serde_json::Value;

use crate::registry::{ResourceInfo, StrategyContext};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Create,
    Update,
    Delete,
    Connect,
}

impl Operation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Operation::Create => "CREATE",
            Operation::Update => "UPDATE",
            Operation::Delete => "DELETE",
            Operation::Connect => "CONNECT",
        }
    }
}

/// `admission.Attributes`
pub struct Attributes<'a> {
    pub operation: Operation,
    pub resource: &'a Arc<ResourceInfo>,
    pub subresource: Option<&'a str>,
    pub namespace: &'a str,
    pub name: &'a str,
    pub old_object: Option<&'a Value>,
    pub dry_run: bool,
}

#[async_trait]
pub trait AdmissionPlugin: Send + Sync {
    fn name(&self) -> &str;

    /// Mutating phase.
    async fn admit(&self, _attrs: &Attributes<'_>, _obj: &mut Value, _ctx: &StrategyContext<'_>) -> Result<(), ApiError> {
        Ok(())
    }

    /// Validating phase (the object must not be changed).
    async fn validate(&self, _attrs: &Attributes<'_>, _obj: &Value, _ctx: &StrategyContext<'_>) -> Result<(), ApiError> {
        Ok(())
    }
}

/// Ordered list of plugins (`--enable-admission-plugins`).
#[derive(Clone, Default)]
pub struct AdmissionChain {
    plugins: Vec<Arc<dyn AdmissionPlugin>>,
}

impl AdmissionChain {
    pub fn new(plugins: Vec<Arc<dyn AdmissionPlugin>>) -> Self {
        Self { plugins }
    }

    pub fn names(&self) -> Vec<String> {
        self.plugins.iter().map(|p| p.name().to_string()).collect()
    }

    pub async fn admit(&self, attrs: &Attributes<'_>, obj: &mut Value, ctx: &StrategyContext<'_>) -> Result<(), ApiError> {
        for p in &self.plugins {
            p.admit(attrs, obj, ctx).await?;
        }
        Ok(())
    }

    pub async fn validate(&self, attrs: &Attributes<'_>, obj: &Value, ctx: &StrategyContext<'_>) -> Result<(), ApiError> {
        for p in &self.plugins {
            p.validate(attrs, obj, ctx).await?;
        }
        Ok(())
    }
}
