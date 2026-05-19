//! New-shape `MarsService` definition + reference wire types.
//!
//! `ClusterRef` points the namespaced `MarsService` at its cluster-scoped
//! `MarsServiceCluster` catalog. `DefinitionSpec` is a sibling-key oneOf
//! (`Volume`-style, not `type:`-discriminated) carrying the
//! `RenderDefinition` source: inline literal, in-cluster `ConfigMap`,
//! git repository, or S3-compatible object store.
//!
//! Mutual exclusivity inside `DefinitionSpec` is enforced by
//! `super::spec::validate_spec`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Pointer to the cluster-scoped `MarsServiceCluster` catalog this
/// `MarsService` composes against.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClusterRef {
    pub(crate) name: String,
}

/// `RenderDefinition` source. Exactly one variant must be set; admission
/// enforced by `super::spec::validate_spec`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DefinitionSpec {
    /// Literal `RenderDefinition` YAML embedded on the CR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) inline: Option<String>,

    /// In-cluster `ConfigMap` carrying the `RenderDefinition` under `key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) config_map_ref: Option<ConfigMapKeyRef>,

    /// Git repository hosting the `RenderDefinition` file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) git_ref: Option<GitRef>,

    /// S3-compatible object store hosting the `RenderDefinition` file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) s3_ref: Option<S3Ref>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConfigMapKeyRef {
    pub(crate) name: String,
    pub(crate) key: String,
}

/// Opaque pointer to a same-namespace `Secret`. Documented key bundle per
/// adapter (see Phase C.4 of the split plan); resolved by the adapter at
/// fetch time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SecretRef {
    pub(crate) name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GitRef {
    pub(crate) url: String,
    pub(crate) git_ref: GitRevision,
    pub(crate) path: String,
    /// Poll cadence. Defaults to `1m` at the adapter when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) interval: Option<String>,
    /// Credential bundle. Omit for public repos.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) secret_ref: Option<SecretRef>,
}

/// Exactly one of `branch`, `tag`, `commit`. Enforced at adapter-resolve time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GitRevision {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) commit: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct S3Ref {
    /// Override for non-AWS endpoints. AWS S3 when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) endpoint: Option<String>,
    pub(crate) region: String,
    pub(crate) bucket: String,
    pub(crate) key: String,
    /// Poll cadence. Defaults to `1m` at the adapter when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) interval: Option<String>,
    /// Credential bundle. Falls back to the default `object_store` chain
    /// (env, IRSA, instance profile) when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) secret_ref: Option<SecretRef>,
}

impl DefinitionSpec {
    /// Number of sibling variants currently set. Used by the admission
    /// validator to enforce exactly-one without naming each branch twice.
    pub(crate) fn variants_set(&self) -> usize {
        usize::from(self.inline.is_some())
            + usize::from(self.config_map_ref.is_some())
            + usize::from(self.git_ref.is_some())
            + usize::from(self.s3_ref.is_some())
    }
}
