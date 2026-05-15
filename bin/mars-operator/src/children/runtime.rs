//! Runtime Deployment. Uses a generic ephemeral volume (per-pod PVC carved
//! from the configured StorageClass) for the local cache.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
use k8s_openapi::api::core::v1::{
    ConfigMapVolumeSource, Container, ContainerPort, EphemeralVolumeSource, HTTPGetAction, PersistentVolumeClaimSpec,
    PersistentVolumeClaimTemplate, PersistentVolumeClaimVolumeSource, PodSpec, PodTemplateSpec, Probe, Volume,
    VolumeMount, VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use crate::children::compiler::{
    container_security_context, env_from, env_vars, pod_security_context, resource_requirements,
};
use crate::children::labels::{
    self, COMPONENT_RUNTIME, CONFIG_CHECKSUM_ANNOTATION, artifact_store_pvc_name, config_map_name,
    runtime_deployment_name,
};
use crate::crd::{ArtifactStoreSpec, MarsService};
use crate::error::Result;

pub(crate) fn build(
    cr: &MarsService,
    config_checksum: &str,
    fs_store: Option<&ArtifactStoreSpec>,
    image: &str,
    owner_ref: OwnerReference,
) -> Result<Deployment> {
    let svc = cr
        .metadata
        .name
        .clone()
        .ok_or_else(|| crate::error::OperatorError::MissingField("metadata.name".into()))?;
    let ns = cr.metadata.namespace.clone();
    let labels_map = labels::labels(&svc, COMPONENT_RUNTIME);

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
            ephemeral: Some(EphemeralVolumeSource {
                volume_claim_template: Some(PersistentVolumeClaimTemplate {
                    metadata: Some(ObjectMeta {
                        labels: Some(labels_map.clone()),
                        ..Default::default()
                    }),
                    spec: PersistentVolumeClaimSpec {
                        access_modes: Some(vec!["ReadWriteOnce".into()]),
                        storage_class_name: if cr.spec.runtime.cache.storage_class.is_empty() {
                            None
                        } else {
                            Some(cr.spec.runtime.cache.storage_class.clone())
                        },
                        resources: Some(VolumeResourceRequirements {
                            requests: Some({
                                let mut m: BTreeMap<String, Quantity> = BTreeMap::new();
                                m.insert("storage".into(), Quantity(cr.spec.runtime.cache.size_limit.clone()));
                                m
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                }),
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
    ];

    if fs_store.is_some() {
        volumes.push(Volume {
            name: "artifact-store".into(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: artifact_store_pvc_name(&svc),
                read_only: Some(true),
            }),
            ..Default::default()
        });
        mounts.push(VolumeMount {
            name: "artifact-store".into(),
            mount_path: "/var/lib/mars/store".into(),
            read_only: Some(true),
            ..Default::default()
        });
    }

    let probe_readiness = Probe {
        http_get: Some(HTTPGetAction {
            path: Some("/readyz".into()),
            port: IntOrString::String("http".into()),
            ..Default::default()
        }),
        period_seconds: Some(5),
        failure_threshold: Some(24),
        ..Default::default()
    };
    let probe_liveness = Probe {
        http_get: Some(HTTPGetAction {
            path: Some("/healthz".into()),
            port: IntOrString::String("http".into()),
            ..Default::default()
        }),
        period_seconds: Some(10),
        failure_threshold: Some(6),
        ..Default::default()
    };

    let container = Container {
        name: "runtime".into(),
        image: Some(image.to_string()),
        args: Some(vec![
            "--mode".into(),
            "runtime".into(),
            "--config".into(),
            "/etc/mars/mars.yaml".into(),
        ]),
        env: Some(env_vars(&cr.spec.runtime.env)),
        env_from: Some(env_from(&cr.spec.runtime.env_from)),
        ports: Some(vec![ContainerPort {
            name: Some("http".into()),
            container_port: cr.spec.runtime.service.port,
            protocol: Some("TCP".into()),
            ..Default::default()
        }]),
        readiness_probe: Some(probe_readiness),
        liveness_probe: Some(probe_liveness),
        resources: cr.spec.runtime.resources.as_ref().map(resource_requirements),
        security_context: Some(container_security_context()),
        volume_mounts: Some(mounts),
        ..Default::default()
    };

    let mut annotations = BTreeMap::new();
    annotations.insert(CONFIG_CHECKSUM_ANNOTATION.into(), config_checksum.to_string());

    let deployment = Deployment {
        metadata: ObjectMeta {
            name: Some(runtime_deployment_name(&svc)),
            namespace: ns,
            labels: Some(labels_map.clone()),
            owner_references: Some(vec![owner_ref]),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(cr.spec.runtime.replicas),
            selector: LabelSelector {
                match_labels: Some(labels::selector(&svc, COMPONENT_RUNTIME)),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels_map),
                    annotations: Some(annotations),
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

    Ok(deployment)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::children::test_support;

    #[test]
    fn build_sets_replicas_and_selector() {
        let cr = test_support::cr("demo", "svc-ns");
        let dep = build(&cr, "abc123", None, test_support::TEST_IMAGE, test_support::owner_ref()).unwrap();
        let spec = dep.spec.unwrap();
        assert_eq!(spec.replicas, Some(2));
        let match_labels = spec.selector.match_labels.unwrap();
        assert_eq!(
            match_labels.get("app.kubernetes.io/component").map(String::as_str),
            Some(COMPONENT_RUNTIME)
        );
        assert_eq!(
            match_labels.get("app.kubernetes.io/instance").map(String::as_str),
            Some("demo")
        );
        assert_eq!(dep.metadata.name.as_deref(), Some("demo-runtime"));
        assert_eq!(dep.metadata.namespace.as_deref(), Some("svc-ns"));
    }

    #[test]
    fn build_propagates_config_checksum_to_pod_template_annotation() {
        let cr = test_support::cr("demo", "svc-ns");
        let dep = build(
            &cr,
            "deadbeef",
            None,
            test_support::TEST_IMAGE,
            test_support::owner_ref(),
        )
        .unwrap();
        let template = dep.spec.unwrap().template;
        let annotations = template.metadata.unwrap().annotations.unwrap();
        assert_eq!(
            annotations.get(CONFIG_CHECKSUM_ANNOTATION).map(String::as_str),
            Some("deadbeef")
        );
    }
}
