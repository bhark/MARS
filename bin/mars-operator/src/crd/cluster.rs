//! Cluster-scoped catalog CR shape: `MarsServiceClusterSpec` (carries the
//! `CustomResource` derive that generates `MarsServiceCluster`). One CR per
//! logical deployment cluster; per-namespace `MarsService` CRs reference it
//! via `spec.clusterRef` to compose their runtime `Config`.
//!
//! Each catalog field is held as an opaque preserve-unknown-fields payload
//! on the wire: the apiserver accepts arbitrary YAML and the operator
//! deserialises into the matching
//! `mars_config` typed struct at reconcile. This keeps the CRD stable across
//! mars-config evolutions and avoids forcing `JsonSchema` derives across the
//! whole config model.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// MarsServiceCluster - one cluster-scoped CR per logical deployment cluster.
#[derive(CustomResource, Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "mars.forn.dk",
    version = "v1alpha1",
    kind = "MarsServiceCluster",
    plural = "marsserviceclusters",
    shortname = "msvcc",
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MarsServiceClusterSpec {
    /// Logical source catalog. Each entry carries a stable `id`; per-service
    /// `MarsService.spec.sources` references those ids. Parsed at reconcile
    /// into `Vec<mars_config::Source>`.
    #[serde(default)]
    #[schemars(schema_with = "super::schema::preserve_unknown_fields_array")]
    pub(crate) sources_catalog: Vec<serde_json::Value>,

    /// Artifact store + cache. Parsed at reconcile into `mars_config::Artifacts`.
    #[schemars(schema_with = "super::schema::preserve_unknown_fields")]
    pub(crate) artifact_store: serde_json::Value,

    /// Cluster-default reprojection allowlist. Narrowed by
    /// `MarsService.spec.reprojection` at compose time via intersection.
    /// Parsed into `mars_config::Reprojection`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "super::schema::preserve_unknown_fields_optional")]
    pub(crate) reprojection: Option<serde_json::Value>,

    /// Cluster-wide observability defaults. Parsed into
    /// `mars_config::Observability`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "super::schema::preserve_unknown_fields_optional")]
    pub(crate) observability: Option<serde_json::Value>,

    /// Cluster-wide compiler/render tuning defaults consumed at compose time.
    #[serde(default)]
    pub(crate) defaults: ClusterDefaults,
}

/// Cluster-wide compiler/render tuning defaults. Each is an opaque payload
/// parsed at reconcile into the matching `mars_config` struct.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClusterDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "super::schema::preserve_unknown_fields_optional")]
    pub(crate) compiler: Option<serde_json::Value>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "super::schema::preserve_unknown_fields_optional")]
    pub(crate) render: Option<serde_json::Value>,
}

// secret-ref + teardown-policy types consumed by
// `MarsServiceCluster.spec.sourcesCatalog[].bootstrap`. kept here because the
// cluster catalog is their only consumer.

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SecretKeyRef {
    pub(crate) name: String,
    pub(crate) key: String,
}

/// Component-style admin credentials reference. Defaults match the
/// `*-superuser` Secret CNPG emits, which is the most common shape in
/// K8s-native Postgres deployments.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AdminCredentialsRef {
    /// Name of the Secret holding the component-style admin credentials.
    pub(crate) secret_name: String,

    #[serde(default = "default_username_key")]
    pub(crate) username_key: String,

    #[serde(default = "default_password_key")]
    pub(crate) password_key: String,

    /// Override key for the host. Falls back to host parsed from the source DSN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) host_key: Option<String>,

    /// Override key for the port. Falls back to port parsed from the source DSN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) port_key: Option<String>,

    /// Override key for the database name. Falls back to dbname parsed from the source DSN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) database_key: Option<String>,
}

fn default_username_key() -> String {
    "username".into()
}

fn default_password_key() -> String {
    "password".into()
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TeardownPolicy {
    #[serde(default = "super::defaults::default_true")]
    pub(crate) slot: bool,
    #[serde(default = "super::defaults::default_true")]
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

#[cfg(test)]
mod tests;
