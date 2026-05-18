//! Top-level CR shape: `MarsServiceSpec` (carries the `CustomResource` derive
//! that generates `MarsService`), `MarsServiceStatus`, `Condition`, and the
//! `print-crd` entry point.
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

use super::bootstrap::BootstrapSpec;
use super::compiler::CompilerSpec;
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

    /// Full MARS service config (mars_config::Config). Opaque in the CRD
    /// schema; parsed and validated server-side at reconcile.
    #[schemars(schema_with = "super::schema::preserve_unknown_fields")]
    pub(crate) config: serde_json::Value,
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

/// Emit the CRD as YAML on stdout. Used by `mars-operator print-crd` and by
/// the chart drift check.
pub(crate) fn print_crd() -> Result<()> {
    let crd = MarsService::crd();
    let yaml = serde_yaml_ng::to_string(&crd).context("serialise CRD as YAML")?;
    print!("{yaml}");
    Ok(())
}
