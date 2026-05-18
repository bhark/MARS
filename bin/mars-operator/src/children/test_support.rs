//! Shared fixtures for child-builder unit tests. Constructs a representative
//! `MarsService` and `OwnerReference` so each builder test stays focused on
//! the wire-shape assertion it actually cares about.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Container, EnvVar, PodSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};

use crate::crd::runtime::RuntimeSpec;
use crate::crd::spec::{MarsService, MarsServiceSpec};

/// `repo:tag` passed to child builders in tests; production builds it once
/// from CLI + CARGO_PKG_VERSION at startup, so tests just need a stable string.
pub(crate) const TEST_IMAGE: &str = "ghcr.io/example/mars:0.0.0-test";

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

/// borrow the `PodSpec` out of a deployment, asserting the surrounding
/// `Option`s are populated. used by child-builder tests to skip the long
/// `dep.spec.unwrap().template.spec.unwrap()` chain.
pub(crate) fn pod_spec(dep: &Deployment) -> &PodSpec {
    dep.spec
        .as_ref()
        .expect("deployment has spec")
        .template
        .spec
        .as_ref()
        .expect("pod template has spec")
}

/// borrow the `PodSpec` out of a job. mirror of `pod_spec` for the
/// bootstrap/teardown job builders.
pub(crate) fn job_pod_spec(job: &Job) -> &PodSpec {
    job.spec
        .as_ref()
        .expect("job has spec")
        .template
        .spec
        .as_ref()
        .expect("pod template has spec")
}

/// look up an env var on a container by name, panicking with a helpful
/// message if absent. callers chase the `value` / `value_from` shape
/// themselves; the helper just trims the find + unwrap noise.
pub(crate) fn env_var<'a>(container: &'a Container, name: &str) -> &'a EnvVar {
    container
        .env
        .as_ref()
        .expect("container has env vars")
        .iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("env var {name} not found"))
}

pub(crate) fn cr(name: &str, namespace: &str) -> MarsService {
    MarsService {
        metadata: ObjectMeta {
            name: Some(name.into()),
            namespace: Some(namespace.into()),
            ..Default::default()
        },
        spec: MarsServiceSpec {
            runtime: RuntimeSpec {
                replicas: 2,
                ..Default::default()
            },
            config: Some(serde_json::json!({
                "service": { "name": "demo" },
                "sources": [{ "id": "default", "kind": "stub" }],
                "artifacts": { "store": { "type": "fs", "path": "/var/lib/mars/artifacts" } }
            })),
            ..Default::default()
        },
        status: None,
    }
}
