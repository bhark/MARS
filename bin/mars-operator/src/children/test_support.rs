//! Shared fixtures for child-builder unit tests. Constructs a representative
//! `MarsService` and `OwnerReference` so each builder test stays focused on
//! the wire-shape assertion it actually cares about.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};

use crate::crd::{MarsService, MarsServiceSpec, RuntimeSpec};

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
            config: serde_json::json!({
                "service": { "name": "demo" },
                "sources": [{ "id": "default", "kind": "stub" }],
                "artifacts": { "store": { "type": "fs", "path": "/var/lib/mars/artifacts" } }
            }),
            ..Default::default()
        },
        status: None,
    }
}
