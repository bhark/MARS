//! Runtime workload CR fields.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::k8s::{EnvFromSourceSpec, EnvVarSpec, ResourceRequirementsSpec, TolerationSpec};

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeSpec {
    #[serde(default = "default_runtime_replicas")]
    pub(crate) replicas: i32,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resources: Option<ResourceRequirementsSpec>,

    #[serde(default)]
    pub(crate) cache: RuntimeCacheSpec,

    #[serde(default)]
    pub(crate) service: RuntimeServiceSpec,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) env: Vec<EnvVarSpec>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) env_from: Vec<EnvFromSourceSpec>,

    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub(crate) node_selector: std::collections::BTreeMap<String, String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) tolerations: Vec<TolerationSpec>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "super::schema::preserve_unknown_fields_optional")]
    pub(crate) affinity: Option<serde_json::Value>,

    /// Extra `corev1.Volume` entries appended to the runtime pod spec after
    /// the operator-managed volumes (config, cache, optional artifact-store).
    /// Opaque in the CRD schema and validated at reconcile time; the names
    /// must not collide with the reserved internals (`config`, `cache`,
    /// `artifact-store`). Primary use: mounting Secrets/ConfigMaps/PVCs of
    /// custom font files so `service.fonts.paths` can pick them up.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(schema_with = "super::schema::preserve_unknown_fields_array")]
    pub(crate) extra_volumes: Vec<serde_json::Value>,

    /// Extra `corev1.VolumeMount` entries appended to the runtime container.
    /// Each entry must reference a volume name defined in `extraVolumes` (or
    /// a reserved internal one). Validated at reconcile time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(schema_with = "super::schema::preserve_unknown_fields_array")]
    pub(crate) extra_volume_mounts: Vec<serde_json::Value>,

    /// Override for the runtime PodDisruptionBudget. When omitted, the
    /// operator auto-creates a PDB with `maxUnavailable: 1` for any
    /// multi-replica runtime (`replicas > 1`) and creates none for a single
    /// replica. Setting this field replaces that default with the given
    /// spec; clearing it restores the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pod_disruption_budget: Option<PodDisruptionBudgetSpec>,
}

/// Schema-friendly mirror of the relevant subset of upstream
/// `PodDisruptionBudgetSpec`. `min_available` and `max_unavailable` are
/// mutually exclusive; the apiserver validates this, so we do not.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PodDisruptionBudgetSpec {
    /// Integer (`"2"`) or percentage string (`"50%"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) min_available: Option<String>,
    /// Integer (`"1"`) or percentage string (`"50%"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_unavailable: Option<String>,
}

fn default_runtime_replicas() -> i32 {
    2
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeCacheSpec {
    #[serde(default = "default_runtime_cache_size")]
    pub(crate) size_limit: String,
    #[serde(default)]
    pub(crate) storage_class: String,
}

impl Default for RuntimeCacheSpec {
    fn default() -> Self {
        Self {
            size_limit: default_runtime_cache_size(),
            storage_class: String::new(),
        }
    }
}

fn default_runtime_cache_size() -> String {
    "1Gi".into()
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeServiceSpec {
    #[serde(default = "default_service_type")]
    pub(crate) service_type: String,
    #[serde(default = "default_service_port")]
    pub(crate) port: i32,
    /// Extra annotations to set on the runtime Service. Useful for ingress
    /// controllers, scrape overrides, cloud-LB hints. Merged on top of any
    /// operator-managed defaults (user keys win on collision).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub(crate) annotations: std::collections::BTreeMap<String, String>,
    /// Emit the standard `prometheus.io/{scrape,port,path}` annotations on
    /// the runtime Service so Prometheus pod/service-monitor scrape discovers
    /// it out of the box. User-supplied `annotations` win on key collision,
    /// so this stays compatible with non-Prometheus stacks (Datadog, etc.).
    #[serde(default = "super::defaults::default_true")]
    pub(crate) metrics_scrape: bool,
}

impl Default for RuntimeServiceSpec {
    fn default() -> Self {
        Self {
            service_type: default_service_type(),
            port: default_service_port(),
            annotations: std::collections::BTreeMap::new(),
            metrics_scrape: true,
        }
    }
}

fn default_service_type() -> String {
    "ClusterIP".into()
}

fn default_service_port() -> i32 {
    8080
}
