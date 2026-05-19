//! Pod-template helpers shared across compiler / runtime Deployments and the
//! cluster-scoped bootstrap Job. Everything here is concerned with the
//! per-pod surface (security contexts, env vars, scheduling fields, opaque
//! volume passthroughs) rather than a specific workload shape.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    Affinity, Capabilities, ConfigMapEnvSource, ConfigMapKeySelector, EnvFromSource, EnvVar, EnvVarSource,
    ObjectFieldSelector, PodSecurityContext, ResourceRequirements, SeccompProfile, SecretEnvSource, SecretKeySelector,
    SecurityContext, Toleration, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

use crate::crd::k8s::{EnvFromSourceSpec, EnvVarSpec, ResourceRequirementsSpec, TolerationSpec};
use crate::error::Result;

pub(crate) fn pod_security_context() -> PodSecurityContext {
    PodSecurityContext {
        run_as_non_root: Some(true),
        run_as_user: Some(65532),
        run_as_group: Some(65532),
        fs_group: Some(65532),
        seccomp_profile: Some(SeccompProfile {
            type_: "RuntimeDefault".into(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub(crate) fn container_security_context() -> SecurityContext {
    SecurityContext {
        allow_privilege_escalation: Some(false),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".into()]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub(crate) fn resource_requirements(spec: &ResourceRequirementsSpec) -> ResourceRequirements {
    let requests: BTreeMap<String, Quantity> = spec
        .requests
        .iter()
        .map(|(k, v)| (k.clone(), Quantity(v.clone())))
        .collect();
    let limits: BTreeMap<String, Quantity> = spec
        .limits
        .iter()
        .map(|(k, v)| (k.clone(), Quantity(v.clone())))
        .collect();
    ResourceRequirements {
        requests: if requests.is_empty() { None } else { Some(requests) },
        limits: if limits.is_empty() { None } else { Some(limits) },
        ..Default::default()
    }
}

pub(crate) fn optional_btree_map(map: &BTreeMap<String, String>) -> Option<BTreeMap<String, String>> {
    if map.is_empty() { None } else { Some(map.clone()) }
}

pub(crate) fn optional_tolerations(specs: &[TolerationSpec]) -> Option<Vec<Toleration>> {
    if specs.is_empty() {
        return None;
    }
    Some(
        specs
            .iter()
            .map(|t| Toleration {
                key: t.key.clone(),
                operator: t.operator.clone(),
                value: t.value.clone(),
                effect: t.effect.clone(),
                toleration_seconds: t.toleration_seconds,
            })
            .collect(),
    )
}

/// Decode an opaque CRD `affinity` value (kube-validated server-side, but
/// we re-validate here so misshapen CRs fail the reconcile with a clear
/// JSON error rather than only at apiserver admission).
pub(crate) fn optional_affinity(value: Option<&serde_json::Value>) -> Result<Option<Affinity>> {
    match value {
        None => Ok(None),
        Some(v) => Ok(Some(serde_json::from_value(v.clone())?)),
    }
}

/// Deserialise an opaque `extraVolumes` array into typed `corev1.Volume`s.
/// Same rationale as `optional_affinity`: validate at reconcile so a typo
/// surfaces in the CR status rather than at first pod admission.
pub(crate) fn extra_volumes(values: &[serde_json::Value]) -> Result<Vec<Volume>> {
    values
        .iter()
        .map(|v| serde_json::from_value(v.clone()).map_err(Into::into))
        .collect()
}

/// Sibling of `extra_volumes` for `extraVolumeMounts`.
pub(crate) fn extra_volume_mounts(values: &[serde_json::Value]) -> Result<Vec<VolumeMount>> {
    values
        .iter()
        .map(|v| serde_json::from_value(v.clone()).map_err(Into::into))
        .collect()
}

pub(crate) fn env_vars(specs: &[EnvVarSpec]) -> Vec<EnvVar> {
    specs
        .iter()
        .map(|e| EnvVar {
            name: e.name.clone(),
            value: e.value.clone(),
            value_from: e.value_from.as_ref().map(|src| EnvVarSource {
                secret_key_ref: src.secret_key_ref.as_ref().map(|k| SecretKeySelector {
                    name: k.name.clone(),
                    key: k.key.clone(),
                    optional: k.optional,
                }),
                config_map_key_ref: src.config_map_key_ref.as_ref().map(|k| ConfigMapKeySelector {
                    name: k.name.clone(),
                    key: k.key.clone(),
                    optional: k.optional,
                }),
                field_ref: src.field_ref.as_ref().map(|f| ObjectFieldSelector {
                    field_path: f.field_path.clone(),
                    api_version: f.api_version.clone(),
                }),
                resource_field_ref: None,
            }),
        })
        .collect()
}

pub(crate) fn env_from(specs: &[EnvFromSourceSpec]) -> Vec<EnvFromSource> {
    specs
        .iter()
        .map(|s| EnvFromSource {
            prefix: s.prefix.clone(),
            secret_ref: s.secret_ref.as_ref().map(|r| SecretEnvSource {
                name: r.name.clone(),
                optional: r.optional,
            }),
            config_map_ref: s.config_map_ref.as_ref().map(|r| ConfigMapEnvSource {
                name: r.name.clone(),
                optional: r.optional,
            }),
        })
        .collect()
}
