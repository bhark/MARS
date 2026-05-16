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
    container_security_context, env_from, env_vars_with_runtime_password, extra_volume_mounts, extra_volumes,
    optional_affinity, optional_btree_map, optional_tolerations, pod_security_context, resource_requirements,
};
use crate::children::labels::{
    self, COMPONENT_RUNTIME, CONFIG_CHECKSUM_ANNOTATION, artifact_store_pvc_name, config_map_name,
    runtime_deployment_name,
};
use crate::crd::{ArtifactStoreSpec, MarsService, SecretKeyRef};
use crate::error::Result;

pub(crate) fn build(
    cr: &MarsService,
    config_checksum: &str,
    fs_store: Option<&ArtifactStoreSpec>,
    runtime_password_ref: Option<&SecretKeyRef>,
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

    // user-supplied entries appended after the managed ones so reserved
    // names (`config`, `cache`, `artifact-store`) stay deterministic.
    volumes.extend(extra_volumes(&cr.spec.runtime.extra_volumes)?);
    mounts.extend(extra_volume_mounts(&cr.spec.runtime.extra_volume_mounts)?);

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
        env: Some(env_vars_with_runtime_password(
            &cr.spec.runtime.env,
            runtime_password_ref,
        )),
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
                    node_selector: optional_btree_map(&cr.spec.runtime.node_selector),
                    tolerations: optional_tolerations(&cr.spec.runtime.tolerations),
                    affinity: optional_affinity(cr.spec.runtime.affinity.as_ref())?,
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
        let dep = build(
            &cr,
            "abc123",
            None,
            None,
            test_support::TEST_IMAGE,
            test_support::owner_ref(),
        )
        .unwrap();
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
    fn build_projects_runtime_password_env_when_resolved_ref_is_set() {
        let cr = test_support::cr("demo", "svc-ns");
        let runtime_ref = SecretKeyRef {
            name: "demo-runtime-credentials".into(),
            key: "password".into(),
        };
        let dep = build(
            &cr,
            "deadbeef",
            None,
            Some(&runtime_ref),
            test_support::TEST_IMAGE,
            test_support::owner_ref(),
        )
        .unwrap();
        let envs = dep.spec.unwrap().template.spec.unwrap().containers[0]
            .env
            .clone()
            .unwrap();
        let injected = envs.iter().find(|e| e.name == "MARS_RUNTIME_PASSWORD").unwrap();
        let sref = injected.value_from.as_ref().unwrap().secret_key_ref.as_ref().unwrap();
        assert_eq!(sref.name, "demo-runtime-credentials");
        assert_eq!(sref.key, "password");
    }

    #[test]
    fn build_propagates_config_checksum_to_pod_template_annotation() {
        let cr = test_support::cr("demo", "svc-ns");
        let dep = build(
            &cr,
            "deadbeef",
            None,
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

    #[test]
    fn build_appends_extra_volumes_and_mounts_after_managed_entries() {
        let mut cr = test_support::cr("demo", "svc-ns");
        cr.spec.runtime.extra_volumes.push(serde_json::json!({
            "name": "fonts",
            "configMap": { "name": "custom-fonts" }
        }));
        cr.spec.runtime.extra_volume_mounts.push(serde_json::json!({
            "name": "fonts",
            "mountPath": "/var/lib/mars/fonts",
            "readOnly": true
        }));
        let dep = build(
            &cr,
            "deadbeef",
            None,
            None,
            test_support::TEST_IMAGE,
            test_support::owner_ref(),
        )
        .unwrap();
        let pod = dep.spec.unwrap().template.spec.unwrap();
        let volumes = pod.volumes.unwrap();
        // user volume appears after the two managed entries (`config`, `cache`).
        assert_eq!(volumes.len(), 3);
        assert_eq!(volumes[0].name, "config");
        assert_eq!(volumes[1].name, "cache");
        assert_eq!(volumes[2].name, "fonts");
        assert_eq!(volumes[2].config_map.as_ref().unwrap().name, "custom-fonts");

        let mounts = pod.containers[0].volume_mounts.clone().unwrap();
        assert_eq!(mounts.len(), 3);
        assert_eq!(mounts[0].name, "config");
        assert_eq!(mounts[1].name, "cache");
        assert_eq!(mounts[2].name, "fonts");
        assert_eq!(mounts[2].mount_path, "/var/lib/mars/fonts");
        assert_eq!(mounts[2].read_only, Some(true));
    }

    #[test]
    fn build_propagates_scheduling_fields_into_pod_spec() {
        use crate::crd::TolerationSpec;
        let mut cr = test_support::cr("demo", "svc-ns");
        cr.spec.runtime.node_selector.insert("zone".into(), "eu-west-1a".into());
        cr.spec.runtime.tolerations.push(TolerationSpec {
            key: Some("dedicated".into()),
            operator: Some("Equal".into()),
            value: Some("runtime".into()),
            effect: Some("NoSchedule".into()),
            toleration_seconds: None,
        });
        cr.spec.runtime.affinity = Some(serde_json::json!({
            "podAntiAffinity": {
                "preferredDuringSchedulingIgnoredDuringExecution": [{
                    "weight": 100,
                    "podAffinityTerm": {
                        "labelSelector": {
                            "matchLabels": { "app.kubernetes.io/component": "runtime" }
                        },
                        "topologyKey": "kubernetes.io/hostname"
                    }
                }]
            }
        }));
        let dep = build(
            &cr,
            "deadbeef",
            None,
            None,
            test_support::TEST_IMAGE,
            test_support::owner_ref(),
        )
        .unwrap();
        let pod = dep.spec.unwrap().template.spec.unwrap();
        assert_eq!(
            pod.node_selector.unwrap().get("zone").map(String::as_str),
            Some("eu-west-1a")
        );
        assert_eq!(pod.tolerations.unwrap()[0].key.as_deref(), Some("dedicated"));
        let pref = pod
            .affinity
            .unwrap()
            .pod_anti_affinity
            .unwrap()
            .preferred_during_scheduling_ignored_during_execution
            .unwrap();
        assert_eq!(pref.len(), 1);
        assert_eq!(pref[0].weight, 100);
    }
}
