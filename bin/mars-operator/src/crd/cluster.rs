//! Cluster-scoped catalog CR shape: `MarsServiceClusterSpec` (carries the
//! `CustomResource` derive that generates `MarsServiceCluster`). One CR per
//! logical deployment cluster; per-namespace `MarsService` CRs reference it
//! via `spec.clusterRef` to compose their runtime `Config`.
//!
//! Each catalog field is held as an opaque preserve-unknown-fields payload
//! on the wire, mirroring `MarsService.spec.config`: the apiserver accepts
//! arbitrary YAML and the operator deserialises into the matching
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

#[cfg(test)]
mod tests;
