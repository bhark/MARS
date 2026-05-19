//! Compiler Deployment + its two PVCs.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy};
use k8s_openapi::api::core::v1::{
    ConfigMapVolumeSource, Container, PersistentVolumeClaim, PersistentVolumeClaimVolumeSource, PodSpec,
    PodTemplateSpec, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};

use crate::children::labels::{
    self, COMPONENT_COMPILER, CONFIG_CHECKSUM_ANNOTATION, compiler_cache_pvc_name, compiler_deployment_name,
    compiler_work_pvc_name, config_map_name,
};
use crate::children::pod::{
    container_security_context, env_from, env_vars, optional_affinity, optional_btree_map, optional_tolerations,
    pod_security_context, resource_requirements,
};
use crate::children::pvc::{self, PvcSpec};
use crate::crd::spec::MarsService;
use crate::error::Result;

pub(crate) struct CompilerChildren {
    pub(crate) deployment: Deployment,
    pub(crate) cache_pvc: PersistentVolumeClaim,
    pub(crate) work_pvc: PersistentVolumeClaim,
}

pub(crate) fn build(
    cr: &MarsService,
    config_checksum: &str,
    fs_store: bool,
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

    if fs_store {
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

#[cfg(test)]
mod tests;
