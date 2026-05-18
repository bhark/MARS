//! schema-friendly mirrors of upstream k8s types. We mirror the structures
//! rather than depend on the k8s-openapi `schemars` feature to keep the CRD
//! stable across k8s-openapi minor bumps. Conversion to the wire type happens
//! at child-render time.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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

/// Schema-friendly mirror of k8s_openapi Toleration. Same rationale as
/// ResourceRequirementsSpec: avoid coupling the CRD schema to the
/// k8s-openapi `schemars` feature.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TolerationSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) operator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) effect: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) toleration_seconds: Option<i64>,
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
