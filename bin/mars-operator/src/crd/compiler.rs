//! Compiler workload CR fields.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::k8s::{EnvFromSourceSpec, EnvVarSpec, ResourceRequirementsSpec, TolerationSpec};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompilerSpec {
    /// Resource requests/limits. Mirrors k8s ResourceRequirements but kept
    /// schema-friendly here so we never tie the CRD to a specific k8s-openapi
    /// minor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resources: Option<ResourceRequirementsSpec>,

    #[serde(default)]
    pub(crate) storage: CompilerStorageSpec,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) env: Vec<EnvVarSpec>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) env_from: Vec<EnvFromSourceSpec>,

    /// Optional ConfigMap (in the same namespace) carrying bitmap files
    /// referenced from styles via `FillPaint::Image { name }`. When set,
    /// the operator mounts it read-only at `/var/lib/mars/images`; the
    /// MARS config must point `compiler.images_dir` at the same path so
    /// the compiler resolves the names during pack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) images_config_map: Option<String>,

    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub(crate) node_selector: std::collections::BTreeMap<String, String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) tolerations: Vec<TolerationSpec>,

    /// Opaque k8s Affinity. Mirrors the upstream `corev1.Affinity` shape;
    /// kube validates the structure server-side. Deserialised at build
    /// time so a malformed CR fails reconcile rather than the apiserver
    /// admission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "super::schema::preserve_unknown_fields_optional")]
    pub(crate) affinity: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompilerStorageSpec {
    #[serde(default = "default_cache_size")]
    pub(crate) cache_size: String,
    #[serde(default = "default_work_size")]
    pub(crate) work_size: String,
    #[serde(default)]
    pub(crate) storage_class: String,
}

impl Default for CompilerStorageSpec {
    fn default() -> Self {
        Self {
            cache_size: default_cache_size(),
            work_size: default_work_size(),
            storage_class: String::new(),
        }
    }
}

fn default_cache_size() -> String {
    "1Gi".into()
}

fn default_work_size() -> String {
    "2Gi".into()
}
