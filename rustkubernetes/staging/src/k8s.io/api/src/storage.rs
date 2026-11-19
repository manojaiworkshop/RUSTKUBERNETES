//! `storage.k8s.io/v1`.

pub mod v1 {
    use std::collections::BTreeMap;

    use k8s_apimachinery::meta::ObjectMeta;
    use serde::{Deserialize, Serialize};
    use serde_json::Value;
    use serde_with::skip_serializing_none;

    use crate::impl_resource;

    /// `StorageClass`: a class of dynamically provisioned (or matched) storage.
    #[skip_serializing_none]
    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct StorageClass {
        pub api_version: Option<String>,
        pub kind: Option<String>,
        #[serde(default)]
        pub metadata: ObjectMeta,
        #[serde(default)]
        pub provisioner: String,
        pub parameters: Option<BTreeMap<String, String>>,
        /// `Delete` (default) or `Retain`.
        pub reclaim_policy: Option<String>,
        pub mount_options: Option<Vec<String>>,
        pub allow_volume_expansion: Option<bool>,
        /// `Immediate` (default) or `WaitForFirstConsumer`.
        pub volume_binding_mode: Option<String>,
        pub allowed_topologies: Option<Vec<Value>>,
    }
    impl_resource!(StorageClass, "storage.k8s.io", "v1", "StorageClass", "storageclasses", false);

    /// `IsDefaultStorageClassAnnotation`
    pub const IS_DEFAULT_CLASS_ANNOTATION: &str = "storageclass.kubernetes.io/is-default-class";
    /// `BetaIsDefaultStorageClassAnnotation`
    pub const BETA_IS_DEFAULT_CLASS_ANNOTATION: &str = "storageclass.beta.kubernetes.io/is-default-class";

    impl StorageClass {
        /// `IsDefaultAnnotation`
        pub fn is_default(&self) -> bool {
            let a = self.metadata.annotations.as_ref();
            a.and_then(|a| a.get(IS_DEFAULT_CLASS_ANNOTATION)).map(String::as_str) == Some("true")
                || a.and_then(|a| a.get(BETA_IS_DEFAULT_CLASS_ANNOTATION)).map(String::as_str) == Some("true")
        }

        pub fn wait_for_first_consumer(&self) -> bool {
            self.volume_binding_mode.as_deref() == Some("WaitForFirstConsumer")
        }
    }
}
