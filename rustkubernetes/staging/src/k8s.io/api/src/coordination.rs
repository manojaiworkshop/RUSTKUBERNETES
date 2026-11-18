//! `coordination.k8s.io/v1`.

pub mod v1 {
    use k8s_apimachinery::meta::ObjectMeta;
    use serde::{Deserialize, Serialize};
    use serde_with::skip_serializing_none;

    use crate::impl_resource;

    /// `Lease` is used for leader election and node heartbeats.
    #[skip_serializing_none]
    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Lease {
        pub api_version: Option<String>,
        pub kind: Option<String>,
        #[serde(default)]
        pub metadata: ObjectMeta,
        pub spec: Option<LeaseSpec>,
    }
    impl_resource!(Lease, "coordination.k8s.io", "v1", "Lease", "leases", true);

    #[skip_serializing_none]
    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct LeaseSpec {
        pub holder_identity: Option<String>,
        pub lease_duration_seconds: Option<i32>,
        /// RFC3339 with microseconds (`MicroTime`).
        pub acquire_time: Option<String>,
        pub renew_time: Option<String>,
        pub lease_transitions: Option<i32>,
    }
}
