//! Shared fixtures for child-builder unit tests. Constructs a representative
//! `MarsService` and `OwnerReference` so each builder test stays focused on
//! the wire-shape assertion it actually cares about.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::PodSpec;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};

use crate::crd::definition::{ClusterRef, DefinitionSpec};
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
            cluster_ref: ClusterRef {
                name: "demo-cluster".into(),
            },
            definition: DefinitionSpec {
                inline: Some("service: { name: demo }\n".into()),
                ..DefinitionSpec::default()
            },
            sources: vec!["default".into()],
            ..Default::default()
        },
        status: None,
    }
}
