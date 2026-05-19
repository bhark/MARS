//! Top-level CR shape: `MarsServiceSpec` (carries the `CustomResource` derive
//! that generates `MarsService`), `MarsServiceStatus`, `Condition`, and the
//! `print-crd` entry point.
//!
//! `spec.config` is the full mars-config Config tree, kept opaque in the CRD
//! schema (x-kubernetes-preserve-unknown-fields: true) and validated by the
//! operator at reconcile time. This keeps the CRD stable across mars-config
//! evolutions without re-publishing the schema for every field tweak.
//!
//! Two admission shapes are accepted: the legacy single-document `config`
//! tree, and the new `clusterRef + definition + sources` triple that composes
//! a `Config` operator-side against a `MarsServiceCluster` catalog. Exactly
//! one of the two shapes must be present; `validate_spec` enforces this.

use anyhow::{Context, Result};
use kube::CustomResource;
use kube::CustomResourceExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::bootstrap::BootstrapSpec;
use super::cluster::MarsServiceCluster;
use super::compiler::CompilerSpec;
use super::definition::{ClusterRef, DefinitionSpec};
use super::runtime::RuntimeSpec;
use super::storage::ArtifactStoreSpec;

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

    /// Legacy single-document MARS service config (mars_config::Config).
    /// Opaque in the CRD schema; parsed and validated server-side at
    /// reconcile. Mutually exclusive with the
    /// `clusterRef + definition + sources` triple.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "super::schema::preserve_unknown_fields_optional")]
    pub(crate) config: Option<serde_json::Value>,

    /// Pointer to the cluster-scoped `MarsServiceCluster` catalog this
    /// service composes against. Required on the new path; rejected on the
    /// legacy path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cluster_ref: Option<ClusterRef>,

    /// `RenderDefinition` source: sibling-key oneOf (`inline`, `configMapRef`,
    /// `gitRef`, `s3Ref`). Required on the new path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) definition: Option<DefinitionSpec>,

    /// Logical source ids to pull from the cluster catalog. Required on the
    /// new path; rejected on the legacy path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sources: Option<Vec<String>>,

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
    /// Name of the Secret holding the runtime role password. Set whether
    /// the user supplied `bootstrap.runtimePasswordSecretRef` (BYO) or the
    /// operator generated and persisted one. Absent when no `spec.bootstrap`
    /// is declared or bootstrap is disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) runtime_credentials_secret: Option<String>,
    /// Name of the operator-managed Secret holding the composed admin DSN.
    /// Set only when the component-style `bootstrap.adminCredentialsRef`
    /// branch is in use; the BYO `adminSecretRef` path leaves this absent
    /// (the user owns that Secret directly).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) bootstrap_admin_credentials_secret: Option<String>,
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
    #[error("spec must set either `config` (legacy) or `clusterRef + definition + sources` (new), not both")]
    BothShapes,
    #[error("spec must set either `config` (legacy) or `clusterRef + definition + sources` (new)")]
    NeitherShape,
    #[error("new-shape spec is missing required field: {0}")]
    NewShapeMissing(&'static str),
    #[error("spec.definition must set exactly one of `inline`, `configMapRef`, `gitRef`, `s3Ref`; found {0}")]
    DefinitionVariantCount(usize),
    #[error(
        "spec.bootstrap is rejected on the new path; bootstrap moved to MarsServiceCluster.spec.sourcesCatalog[].bootstrap"
    )]
    BootstrapOnNewPath,
}

/// Enforce the dual-shape admission rule: exactly one of (`config`) or
/// (`clusterRef + definition + sources`), and inside `definition` exactly one
/// sibling variant. Run at the top of reconcile.
pub(crate) fn validate_spec(spec: &MarsServiceSpec) -> Result<(), SpecValidationError> {
    let legacy = spec.config.is_some();
    let new_any = spec.cluster_ref.is_some() || spec.definition.is_some() || spec.sources.is_some();

    if legacy && new_any {
        return Err(SpecValidationError::BothShapes);
    }
    if !legacy && !new_any {
        return Err(SpecValidationError::NeitherShape);
    }

    if !legacy {
        if spec.cluster_ref.is_none() {
            return Err(SpecValidationError::NewShapeMissing("clusterRef"));
        }
        let Some(def) = spec.definition.as_ref() else {
            return Err(SpecValidationError::NewShapeMissing("definition"));
        };
        if spec.sources.is_none() {
            return Err(SpecValidationError::NewShapeMissing("sources"));
        }
        let count = def.variants_set();
        if count != 1 {
            return Err(SpecValidationError::DefinitionVariantCount(count));
        }
        // bootstrap is a cluster-side concern on the new path
        if spec.bootstrap.is_some() {
            return Err(SpecValidationError::BootstrapOnNewPath);
        }
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
