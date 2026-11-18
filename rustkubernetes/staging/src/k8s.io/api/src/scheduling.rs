//! `scheduling.k8s.io/v1`.

pub mod v1 {
    use k8s_apimachinery::meta::ObjectMeta;
    use serde::{Deserialize, Serialize};
    use serde_with::skip_serializing_none;

    use crate::impl_resource;

    /// Maps a priority class name to an integer priority (`PriorityClass`).
    #[skip_serializing_none]
    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct PriorityClass {
        pub api_version: Option<String>,
        pub kind: Option<String>,
        #[serde(default)]
        pub metadata: ObjectMeta,
        #[serde(default)]
        pub value: i32,
        pub global_default: Option<bool>,
        pub description: Option<String>,
        /// `PreemptLowerPriority` (default) or `Never`
        pub preemption_policy: Option<String>,
    }
    impl_resource!(PriorityClass, "scheduling.k8s.io", "v1", "PriorityClass", "priorityclasses", false);

    pub const SYSTEM_CLUSTER_CRITICAL: &str = "system-cluster-critical";
    pub const SYSTEM_NODE_CRITICAL: &str = "system-node-critical";
    /// Values above this are reserved for system classes.
    pub const HIGHEST_USER_DEFINABLE_PRIORITY: i32 = 1_000_000_000;
}
