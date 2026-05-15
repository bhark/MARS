//! Compiler Deployment + its two PVCs.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy};
use k8s_openapi::api::core::v1::{
    ConfigMapEnvSource, ConfigMapKeySelector, ConfigMapVolumeSource, Container, EnvFromSource, EnvVar, EnvVarSource,
    ObjectFieldSelector, PersistentVolumeClaim, PersistentVolumeClaimVolumeSource, PodSecurityContext, PodSpec,
    PodTemplateSpec, ResourceRequirements, SeccompProfile, SecretEnvSource, SecretKeySelector, SecurityContext, Volume,
    VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};

use crate::children::labels::{
    self, COMPONENT_COMPILER, CONFIG_CHECKSUM_ANNOTATION, compiler_cache_pvc_name, compiler_deployment_name,
    compiler_work_pvc_name, config_map_name,
};
use crate::children::pvc::{self, PvcSpec};
use crate::crd::{ArtifactStoreSpec, EnvFromSourceSpec, EnvVarSpec, MarsService, ResourceRequirementsSpec};
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
        env: Some(env_vars(&cr.spec.compiler.env)),
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
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::children::test_support;

    #[test]
    fn build_yields_three_children_named_per_instance() {
        let cr = test_support::cr("demo", "svc-ns");
        let kids = build(
            &cr,
            "deadbeef",
            None,
            test_support::TEST_IMAGE,
            test_support::owner_ref(),
        )
        .unwrap();
        assert_eq!(kids.deployment.metadata.name.as_deref(), Some("demo-compiler"));
        assert_eq!(kids.cache_pvc.metadata.name.as_deref(), Some("demo-compiler-cache"));
        assert_eq!(kids.work_pvc.metadata.name.as_deref(), Some("demo-compiler-work"));
        // every child is in the CR's namespace
        for ns in [
            kids.deployment.metadata.namespace.as_deref(),
            kids.cache_pvc.metadata.namespace.as_deref(),
            kids.work_pvc.metadata.namespace.as_deref(),
        ] {
            assert_eq!(ns, Some("svc-ns"));
        }
    }

    #[test]
    fn build_propagates_config_checksum_to_pod_template_annotation() {
        let cr = test_support::cr("demo", "svc-ns");
        let kids = build(&cr, "abc123", None, test_support::TEST_IMAGE, test_support::owner_ref()).unwrap();
        let template = kids.deployment.spec.unwrap().template;
        let annotations = template.metadata.unwrap().annotations.unwrap();
        assert_eq!(
            annotations.get(CONFIG_CHECKSUM_ANNOTATION).map(String::as_str),
            Some("abc123")
        );
    }

    #[test]
    fn build_missing_metadata_name_errors() {
        let mut cr = test_support::cr("demo", "svc-ns");
        cr.metadata.name = None;
        // CompilerChildren is intentionally not Debug; use match to extract the err.
        match build(&cr, "abc123", None, test_support::TEST_IMAGE, test_support::owner_ref()) {
            Err(crate::error::OperatorError::MissingField(f)) => assert_eq!(f, "metadata.name"),
            Err(other) => panic!("expected MissingField, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[test]
    fn build_without_images_config_map_omits_images_volume() {
        let cr = test_support::cr("demo", "svc-ns");
        let kids = build(
            &cr,
            "deadbeef",
            None,
            test_support::TEST_IMAGE,
            test_support::owner_ref(),
        )
        .unwrap();
        let pod = kids.deployment.spec.unwrap().template.spec.unwrap();
        assert!(pod.volumes.unwrap().iter().all(|v| v.name != "images"));
        let mounts = pod.containers[0].volume_mounts.as_ref().unwrap();
        assert!(mounts.iter().all(|m| m.name != "images"));
    }

    #[test]
    fn build_with_images_config_map_mounts_read_only() {
        let mut cr = test_support::cr("demo", "svc-ns");
        cr.spec.compiler.images_config_map = Some("mars-images".into());
        let kids = build(
            &cr,
            "deadbeef",
            None,
            test_support::TEST_IMAGE,
            test_support::owner_ref(),
        )
        .unwrap();
        let pod = kids.deployment.spec.unwrap().template.spec.unwrap();
        let vol = pod.volumes.unwrap().into_iter().find(|v| v.name == "images").unwrap();
        let cm = vol.config_map.unwrap();
        assert_eq!(cm.name, "mars-images");
        let mount = pod.containers[0]
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|m| m.name == "images")
            .unwrap();
        assert_eq!(mount.mount_path, "/var/lib/mars/images");
        assert_eq!(mount.read_only, Some(true));
    }
}
