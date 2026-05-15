//! MarsService CustomResource definition.
//!
//! `spec.config` is the full mars-config Config tree, kept opaque in the CRD
//! schema (x-kubernetes-preserve-unknown-fields: true) and validated by the
//! operator at reconcile time. This keeps the CRD stable across mars-config
//! evolutions without re-publishing the schema for every field tweak.

use anyhow::{Context, Result};
use kube::CustomResource;
use kube::CustomResourceExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// MarsService - one CR per logical MARS service in a namespace.
#[derive(CustomResource, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "mars.forn.dk",
    version = "v1alpha1",
    kind = "MarsService",
    plural = "marsservices",
    shortname = "msvc",
    namespaced,
    status = "MarsServiceStatus",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MarsServiceSpec {
    pub(crate) compiler: CompilerSpec,

    pub(crate) runtime: RuntimeSpec,

    /// Only consulted when `spec.config.artifacts.store.type == "fs"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) artifact_store: Option<ArtifactStoreSpec>,

    /// Postgres catalog bootstrap. When present and `enabled` is true (the
    /// default), the operator runs a one-shot Job that calls `mars setup`
    /// before any compiler/runtime workload comes up. Names + schemas are
    /// declared inside `spec.config.source.bootstrap` (and used both by the
    /// Job and by `mars setup` for bare-metal deployments); this block only
    /// carries the Kubernetes-specific orchestration knobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) bootstrap: Option<BootstrapSpec>,

    /// Full MARS service config (mars_config::Config). Opaque in the CRD
    /// schema; parsed and validated server-side at reconcile.
    #[schemars(schema_with = "preserve_unknown_fields")]
    pub(crate) config: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BootstrapSpec {
    /// When false, the operator skips Job-driven bootstrap and runs a read-
    /// only preflight against postgres with the runtime credential, gating
    /// the compiler/runtime children on the prerequisites already existing.
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,

    /// Secret reference for the admin DSN (CREATE ROLE / CREATE PUBLICATION /
    /// pg_create_logical_replication_slot privileges). Required when
    /// `enabled` is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) admin_secret_ref: Option<SecretKeyRef>,

    /// Secret reference for the runtime role password. Required when
    /// `enabled` is true. Mounted only into the bootstrap Job pod; the
    /// always-on compiler/runtime never sees the admin secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) runtime_password_secret_ref: Option<SecretKeyRef>,

    /// What to drop on CR delete. Role removal defaults off so shared roles
    /// survive a service teardown.
    #[serde(default)]
    pub(crate) teardown_on_delete: TeardownPolicy,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SecretKeyRef {
    pub(crate) name: String,
    pub(crate) key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TeardownPolicy {
    #[serde(default = "default_true")]
    pub(crate) slot: bool,
    #[serde(default = "default_true")]
    pub(crate) publication: bool,
    #[serde(default)]
    pub(crate) role: bool,
}

impl Default for TeardownPolicy {
    fn default() -> Self {
        Self {
            slot: true,
            publication: true,
            role: false,
        }
    }
}

fn default_true() -> bool {
    true
}

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

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
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
}

impl Default for RuntimeServiceSpec {
    fn default() -> Self {
        Self {
            service_type: default_service_type(),
            port: default_service_port(),
        }
    }
}

fn default_service_type() -> String {
    "ClusterIP".into()
}

fn default_service_port() -> i32 {
    8080
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArtifactStoreSpec {
    #[serde(default = "default_artifact_size")]
    pub(crate) size: String,
    #[serde(default)]
    pub(crate) storage_class: String,
    #[serde(default = "default_access_modes")]
    pub(crate) access_modes: Vec<String>,
}

impl Default for ArtifactStoreSpec {
    fn default() -> Self {
        Self {
            size: default_artifact_size(),
            storage_class: String::new(),
            access_modes: default_access_modes(),
        }
    }
}

fn default_artifact_size() -> String {
    "5Gi".into()
}

fn default_access_modes() -> Vec<String> {
    vec!["ReadWriteOnce".into()]
}

/// Schema-friendly mirror of k8s_openapi ResourceRequirements. Conversion to
/// the wire type happens at child-render time. We mirror the structure rather
/// than depend on the k8s-openapi `schemars` feature to keep the CRD stable
/// across k8s-openapi minor bumps.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResourceRequirementsSpec {
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub(crate) requests: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub(crate) limits: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnvVarSpec {
    pub(crate) name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) value_from: Option<EnvVarSourceSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnvVarSourceSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) secret_key_ref: Option<KeySelectorSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) config_map_key_ref: Option<KeySelectorSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) field_ref: Option<ObjectFieldSelectorSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct KeySelectorSpec {
    pub(crate) name: String,
    pub(crate) key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) optional: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ObjectFieldSelectorSpec {
    pub(crate) field_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) api_version: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnvFromSourceSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) secret_ref: Option<LocalObjectReferenceSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) config_map_ref: Option<LocalObjectReferenceSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalObjectReferenceSpec {
    pub(crate) name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) optional: Option<bool>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MarsServiceStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) observed_generation: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) conditions: Vec<Condition>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Condition {
    #[serde(rename = "type")]
    pub(crate) type_: String,
    pub(crate) status: String,
    pub(crate) reason: String,
    pub(crate) message: String,
    pub(crate) last_transition_time: String,
}

/// Schema function for `spec.config`: emits `{type: object,
/// x-kubernetes-preserve-unknown-fields: true}` so the apiserver accepts any
/// shape under that key. The operator does real validation at reconcile.
fn preserve_unknown_fields(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut map = serde_json::Map::new();
    map.insert("type".into(), serde_json::Value::String("object".into()));
    map.insert(
        "x-kubernetes-preserve-unknown-fields".into(),
        serde_json::Value::Bool(true),
    );
    schemars::Schema::from(map)
}

/// Emit the CRD as YAML on stdout. Used by `mars-operator print-crd` and by
/// the chart drift check.
pub(crate) fn print_crd() -> Result<()> {
    let crd = MarsService::crd();
    let yaml = serde_yaml_ng::to_string(&crd).context("serialise CRD as YAML")?;
    print!("{yaml}");
    Ok(())
}
