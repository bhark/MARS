//! Compiler Deployment + its two PVCs.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy};
use k8s_openapi::api::core::v1::{
    Affinity, ConfigMapEnvSource, ConfigMapKeySelector, ConfigMapVolumeSource, Container, EnvFromSource, EnvVar,
    EnvVarSource, ObjectFieldSelector, PersistentVolumeClaim, PersistentVolumeClaimVolumeSource, PodSecurityContext,
    PodSpec, PodTemplateSpec, ResourceRequirements, SeccompProfile, SecretEnvSource, SecretKeySelector,
    SecurityContext, Toleration, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};

use crate::children::labels::{
    self, COMPONENT_COMPILER, CONFIG_CHECKSUM_ANNOTATION, RUNTIME_PASSWORD_ENV, compiler_cache_pvc_name,
    compiler_deployment_name, compiler_work_pvc_name, config_map_name,
};
use crate::children::pvc::{self, PvcSpec};
use crate::crd::bootstrap::SecretKeyRef;
use crate::crd::k8s::{EnvFromSourceSpec, EnvVarSpec, ResourceRequirementsSpec, TolerationSpec};
use crate::crd::spec::MarsService;
use crate::crd::storage::ArtifactStoreSpec;
use crate::error::Result;

pub(crate) struct CompilerChildren {
    pub(crate) deployment: Deployment,
    pub(crate) cache_pvc: PersistentVolumeClaim,
    pub(crate) work_pvc: PersistentVolumeClaim,
}

pub(crate) fn build(
    cr: &MarsService,
    config_checksum: &str,
    fs_store: Option<&ArtifactStoreSpec>,
    runtime_password_ref: Option<&SecretKeyRef>,
    image: &str,
    owner_ref: OwnerReference,
) -> Result<CompilerChildren> {
    let svc = cr
        .metadata
        .name
        .clone()
        .ok_or_else(|| crate::error::OperatorError::MissingField("metadata.name".into()))?;
    let ns = cr.metadata.namespace.clone();

    let labels_map = labels::labels(&svc, COMPONENT_COMPILER);

    let cache_pvc = pvc::build(
        PvcSpec {
            name: &compiler_cache_pvc_name(&svc),
            namespace: ns.as_deref(),
            labels: labels_map.clone(),
            size: &cr.spec.compiler.storage.cache_size,
            storage_class: &cr.spec.compiler.storage.storage_class,
            access_modes: &["ReadWriteOnce".into()],
        },
        owner_ref.clone(),
    );

    let work_pvc = pvc::build(
        PvcSpec {
            name: &compiler_work_pvc_name(&svc),
            namespace: ns.as_deref(),
            labels: labels_map.clone(),
            size: &cr.spec.compiler.storage.work_size,
            storage_class: &cr.spec.compiler.storage.storage_class,
            access_modes: &["ReadWriteOnce".into()],
        },
        owner_ref.clone(),
    );

    let mut volumes = vec![
        Volume {
            name: "config".into(),
            config_map: Some(ConfigMapVolumeSource {
                name: config_map_name(&svc),
                ..Default::default()
            }),
            ..Default::default()
        },
        Volume {
            name: "cache".into(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: compiler_cache_pvc_name(&svc),
                read_only: Some(false),
            }),
            ..Default::default()
        },
        Volume {
            name: "work".into(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: compiler_work_pvc_name(&svc),
                read_only: Some(false),
            }),
            ..Default::default()
        },
    ];

    let mut mounts = vec![
        VolumeMount {
            name: "config".into(),
            mount_path: "/etc/mars/mars.yaml".into(),
            sub_path: Some("mars.yaml".into()),
            read_only: Some(true),
            ..Default::default()
        },
        VolumeMount {
            name: "cache".into(),
            mount_path: "/cache".into(),
            ..Default::default()
        },
        VolumeMount {
            name: "work".into(),
            mount_path: "/work".into(),
            ..Default::default()
        },
    ];

    if fs_store.is_some() {
        volumes.push(Volume {
            name: "artifact-store".into(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: labels::artifact_store_pvc_name(&svc),
                read_only: Some(false),
            }),
            ..Default::default()
        });
        mounts.push(VolumeMount {
            name: "artifact-store".into(),
            mount_path: "/var/lib/mars/store".into(),
            ..Default::default()
        });
    }

    if let Some(name) = cr.spec.compiler.images_config_map.as_deref() {
        volumes.push(Volume {
            name: "images".into(),
            config_map: Some(ConfigMapVolumeSource {
                name: name.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        });
        mounts.push(VolumeMount {
            name: "images".into(),
            mount_path: "/var/lib/mars/images".into(),
            read_only: Some(true),
            ..Default::default()
        });
    }

    let container = Container {
        name: "compiler".into(),
        image: Some(image.to_string()),
        args: Some(vec![
            "--mode".into(),
            "compiler".into(),
            "--config".into(),
            "/etc/mars/mars.yaml".into(),
        ]),
        env: Some(env_vars_with_runtime_password(
            &cr.spec.compiler.env,
            runtime_password_ref,
        )),
        env_from: Some(env_from(&cr.spec.compiler.env_from)),
        resources: cr.spec.compiler.resources.as_ref().map(resource_requirements),
        security_context: Some(container_security_context()),
        volume_mounts: Some(mounts),
        ..Default::default()
    };

    let mut pod_annotations = BTreeMap::new();
    pod_annotations.insert(CONFIG_CHECKSUM_ANNOTATION.into(), config_checksum.to_string());

    let deployment = Deployment {
        metadata: ObjectMeta {
            name: Some(compiler_deployment_name(&svc)),
            namespace: ns.clone(),
            labels: Some(labels_map.clone()),
            owner_references: Some(vec![owner_ref]),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(1),
            strategy: Some(DeploymentStrategy {
                type_: Some("Recreate".into()),
                rolling_update: None,
            }),
            selector: LabelSelector {
                match_labels: Some(labels::selector(&svc, COMPONENT_COMPILER)),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels_map),
                    annotations: Some(pod_annotations),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    security_context: Some(pod_security_context()),
                    containers: vec![container],
                    volumes: Some(volumes),
                    node_selector: optional_btree_map(&cr.spec.compiler.node_selector),
                    tolerations: optional_tolerations(&cr.spec.compiler.tolerations),
                    affinity: optional_affinity(cr.spec.compiler.affinity.as_ref())?,
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    };

    Ok(CompilerChildren {
        deployment,
        cache_pvc,
        work_pvc,
    })
}

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
        capabilities: Some(k8s_openapi::api::core::v1::Capabilities {
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

/// Append the operator-projected `MARS_RUNTIME_PASSWORD` env to the
/// user-supplied env list. The projection is the resolved runtime password
/// Secret (BYO or operator-managed). Users template it into their DSN via
/// `${MARS_RUNTIME_PASSWORD}` so the always-on pods can authenticate as the
/// runtime role without the user staging a password Secret themselves.
pub(crate) fn env_vars_with_runtime_password(
    specs: &[EnvVarSpec],
    runtime_password_ref: Option<&SecretKeyRef>,
) -> Vec<EnvVar> {
    let mut out = env_vars(specs);
    if let Some(r) = runtime_password_ref
        && !out.iter().any(|e| e.name == RUNTIME_PASSWORD_ENV)
    {
        out.push(EnvVar {
            name: RUNTIME_PASSWORD_ENV.into(),
            value_from: Some(EnvVarSource {
                secret_key_ref: Some(SecretKeySelector {
                    name: r.name.clone(),
                    key: r.key.clone(),
                    optional: Some(false),
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    out
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

#[cfg(test)]
mod tests;
