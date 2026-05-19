//! Top-level CR shape: `MarsServiceSpec` (carries the `CustomResource` derive
//! that generates `MarsService`), `MarsServiceStatus`, `Condition`, and the
//! `print-crd` entry point.
//!
//! The service is composed at reconcile from a cluster-scoped
//! `MarsServiceCluster` catalog plus a `RenderDefinition` resolved through the
//! `spec.definition` sibling-key oneOf (`inline | configMapRef | gitRef | s3Ref`).

use anyhow::{Context, Result};
use kube::CustomResource;
use kube::CustomResourceExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::cluster::MarsServiceCluster;
use super::compiler::CompilerSpec;
use super::definition::{ClusterRef, DefinitionSpec};
use super::runtime::RuntimeSpec;

/// MarsService - one CR per logical MARS service in a namespace.
#[derive(CustomResource, Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
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

    /// Pointer to the cluster-scoped `MarsServiceCluster` catalog this service
    /// composes against.
    pub(crate) cluster_ref: ClusterRef,

    /// `RenderDefinition` source: sibling-key oneOf (`inline`, `configMapRef`,
    /// `gitRef`, `s3Ref`). Exactly one variant must be set.
    pub(crate) definition: DefinitionSpec,

    /// Logical source ids to pull from the cluster catalog. Each entry must
    /// resolve to a `sourcesCatalog[].id` on the referenced cluster.
    pub(crate) sources: Vec<String>,

    /// Service-side reprojection narrowing. Intersected against the cluster
    /// default at compose time. Parsed into `mars_config::Reprojection` at
    /// reconcile; kept opaque in the CRD schema for the same reason as
    /// `MarsServiceCluster.spec.reprojection`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "super::schema::preserve_unknown_fields_optional")]
    pub(crate) reprojection: Option<serde_json::Value>,
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
    /// Identity of the last successfully fetched RenderDefinition. Cleared
    /// while `DefinitionResolved` is not True.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) definition: Option<DefinitionStatus>,
}

/// Status block reporting the operator's view of the resolved render
/// definition.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DefinitionStatus {
    pub(crate) observed: DefinitionObserved,
}

/// Identity of the most recently fetched RenderDefinition. `adapter` is one
/// of `inline` / `configMapRef` / `gitRef` / `s3Ref`; `revision` is the
/// adapter-stable identity (sha, etag, resourceVersion, content hash).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DefinitionObserved {
    pub(crate) adapter: String,
    pub(crate) revision: String,
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

/// Admission-time validation of `MarsServiceSpec`. Returned errors are typed
/// so call sites can map them onto status conditions without string sniffing.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SpecValidationError {
    #[error("spec.definition must set exactly one of `inline`, `configMapRef`, `gitRef`, `s3Ref`; found {0}")]
    DefinitionVariantCount(usize),
}

/// Enforce `definition` exactly-one. Other required fields are guaranteed by
/// the serde shape — apiserver admission rejects a CR missing them.
pub(crate) fn validate_spec(spec: &MarsServiceSpec) -> Result<(), SpecValidationError> {
    let count = spec.definition.variants_set();
    if count != 1 {
        return Err(SpecValidationError::DefinitionVariantCount(count));
    }
    Ok(())
}

/// Emit both CRDs as a multi-doc YAML stream on stdout. Used by
/// `mars-operator print-crd` and by the chart drift check. `MarsServiceCluster`
/// is emitted first because `MarsService.spec.clusterRef` references it, and
/// Helm installs CRDs in file order.
pub(crate) fn print_crd() -> Result<()> {
    let cluster = serde_yaml_ng::to_string(&MarsServiceCluster::crd()).context("serialise MarsServiceCluster CRD")?;
    let service = serde_yaml_ng::to_string(&MarsService::crd()).context("serialise MarsService CRD")?;
    print!("{cluster}---\n{service}");
    Ok(())
}

#[cfg(test)]
mod tests;
