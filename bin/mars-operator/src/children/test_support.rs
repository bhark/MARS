//! Shared fixtures for child-builder unit tests. Constructs a representative
//! `MarsService` and `OwnerReference` so each builder test stays focused on
//! the wire-shape assertion it actually cares about.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};

use crate::crd::{
    CompilerSpec, CompilerStorageSpec, ImageSpec, MarsService, MarsServiceSpec, RuntimeCacheSpec, RuntimeServiceSpec,
    RuntimeSpec,
};

pub(crate) fn owner_ref() -> OwnerReference {
    OwnerReference {
        api_version: "mars.forn.dk/v1alpha1".into(),
        kind: "MarsService".into(),
        name: "demo".into(),
        uid: "00000000-0000-0000-0000-000000000001".into(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    }
}

pub(crate) fn cr(name: &str, namespace: &str) -> MarsService {
    MarsService {
        metadata: ObjectMeta {
            name: Some(name.into()),
            namespace: Some(namespace.into()),
            ..Default::default()
        },
        spec: MarsServiceSpec {
            image: ImageSpec {
                repository: "ghcr.io/example/mars".into(),
                tag: "v0.0.1".into(),
                pull_policy: "IfNotPresent".into(),
            },
            compiler: CompilerSpec {
                resources: None,
                storage: CompilerStorageSpec {
                    cache_size: "1Gi".into(),
                    work_size: "2Gi".into(),
                    storage_class: String::new(),
                },
                env: Vec::new(),
                env_from: Vec::new(),
                images_config_map: None,
            },
            runtime: RuntimeSpec {
                replicas: 2,
                resources: None,
                cache: RuntimeCacheSpec::default(),
                service: RuntimeServiceSpec::default(),
                env: Vec::new(),
                env_from: Vec::new(),
            },
            artifact_store: None,
            config: serde_json::json!({
                "service": { "name": "demo" },
                "source":  { "kind": "stub" },
                "artifacts": { "store": { "type": "fs", "path": "/var/lib/mars/artifacts" } }
            }),
        },
        status: None,
    }
}
