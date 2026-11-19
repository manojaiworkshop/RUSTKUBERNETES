//! Port of `pkg/controller/nodeipam/ipam`: CIDR allocators for `node.spec.podCIDRs`.

pub mod cidrset;
pub mod range_allocator;

/// `--cidr-allocator-type` (`ipam.CIDRAllocatorType`).
pub const RANGE_ALLOCATOR_TYPE: &str = "RangeAllocator";
pub const CLOUD_ALLOCATOR_TYPE: &str = "CloudAllocator";

/// Attempts at patching a node's CIDRs (`cidrUpdateRetries`).
pub const CIDR_UPDATE_RETRIES: usize = 3;
