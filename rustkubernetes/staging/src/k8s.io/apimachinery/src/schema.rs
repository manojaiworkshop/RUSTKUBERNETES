//! Port of `k8s.io/apimachinery/pkg/runtime/schema`.

use std::fmt;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GroupVersion {
    pub group: String,
    pub version: String,
}

impl GroupVersion {
    pub fn new(group: &str, version: &str) -> Self {
        Self { group: group.into(), version: version.into() }
    }

    /// Parses `apps/v1` or `v1`.
    pub fn parse(api_version: &str) -> Self {
        match api_version.split_once('/') {
            Some((g, v)) => Self::new(g, v),
            None => Self::new("", api_version),
        }
    }

    /// `v1` for the core group, `apps/v1` otherwise.
    pub fn api_version(&self) -> String {
        if self.group.is_empty() {
            self.version.clone()
        } else {
            format!("{}/{}", self.group, self.version)
        }
    }
}

impl fmt::Display for GroupVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.api_version())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GroupVersionKind {
    pub group: String,
    pub version: String,
    pub kind: String,
}

impl GroupVersionKind {
    pub fn new(group: &str, version: &str, kind: &str) -> Self {
        Self { group: group.into(), version: version.into(), kind: kind.into() }
    }

    pub fn group_version(&self) -> GroupVersion {
        GroupVersion::new(&self.group, &self.version)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GroupVersionResource {
    pub group: String,
    pub version: String,
    pub resource: String,
}

impl GroupVersionResource {
    pub fn new(group: &str, version: &str, resource: &str) -> Self {
        Self { group: group.into(), version: version.into(), resource: resource.into() }
    }

    pub fn group_version(&self) -> GroupVersion {
        GroupVersion::new(&self.group, &self.version)
    }

    /// `pods` or `deployments.apps`
    pub fn group_resource(&self) -> String {
        if self.group.is_empty() {
            self.resource.clone()
        } else {
            format!("{}.{}", self.resource, self.group)
        }
    }
}
