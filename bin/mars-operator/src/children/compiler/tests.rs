#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;
use crate::children::test_support;

#[test]
fn build_yields_three_children_named_per_instance() {
    let cr = test_support::cr("demo", "svc-ns");
    let kids = build(
        &cr,
        "deadbeef",
        None,
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
    let kids = build(
        &cr,
        "abc123",
        None,
        None,
        test_support::TEST_IMAGE,
        test_support::owner_ref(),
    )
    .unwrap();
    let template = &kids.deployment.spec.as_ref().unwrap().template;
    let annotations = template.metadata.as_ref().unwrap().annotations.as_ref().unwrap();
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
    match build(
        &cr,
        "abc123",
        None,
        None,
        test_support::TEST_IMAGE,
        test_support::owner_ref(),
    ) {
        Err(crate::error::OperatorError::MissingField(f)) => assert_eq!(f, "metadata.name"),
        Err(other) => panic!("expected MissingField, got {other:?}"),
        Ok(_) => panic!("expected error, got Ok"),
    }
}

#[test]
fn build_projects_runtime_password_env_when_resolved_ref_is_set() {
    let cr = test_support::cr("demo", "svc-ns");
    let runtime_ref = SecretKeyRef {
        name: "demo-runtime-credentials".into(),
        key: "password".into(),
    };
    let kids = build(
        &cr,
        "deadbeef",
        None,
        Some(&runtime_ref),
        test_support::TEST_IMAGE,
        test_support::owner_ref(),
    )
    .unwrap();
    let pod = test_support::pod_spec(&kids.deployment);
    let injected = test_support::env_var(&pod.containers[0], RUNTIME_PASSWORD_ENV);
    let sref = injected.value_from.as_ref().unwrap().secret_key_ref.as_ref().unwrap();
    assert_eq!(sref.name, "demo-runtime-credentials");
    assert_eq!(sref.key, "password");
}

#[test]
fn build_omits_runtime_password_env_when_resolved_ref_is_absent() {
    let cr = test_support::cr("demo", "svc-ns");
    let kids = build(
        &cr,
        "deadbeef",
        None,
        None,
        test_support::TEST_IMAGE,
        test_support::owner_ref(),
    )
    .unwrap();
    let pod = test_support::pod_spec(&kids.deployment);
    let envs = pod.containers[0].env.as_ref().unwrap();
    assert!(envs.iter().all(|e| e.name != RUNTIME_PASSWORD_ENV));
}

#[test]
fn build_without_images_config_map_omits_images_volume() {
    let cr = test_support::cr("demo", "svc-ns");
    let kids = build(
        &cr,
        "deadbeef",
        None,
        None,
        test_support::TEST_IMAGE,
        test_support::owner_ref(),
    )
    .unwrap();
    let pod = test_support::pod_spec(&kids.deployment);
    assert!(pod.volumes.as_ref().unwrap().iter().all(|v| v.name != "images"));
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
        None,
        test_support::TEST_IMAGE,
        test_support::owner_ref(),
    )
    .unwrap();
    let pod = test_support::pod_spec(&kids.deployment);
    let vol = pod
        .volumes
        .as_ref()
        .unwrap()
        .iter()
        .find(|v| v.name == "images")
        .unwrap();
    let cm = vol.config_map.as_ref().unwrap();
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

#[test]
fn build_omits_scheduling_fields_when_unset() {
    let cr = test_support::cr("demo", "svc-ns");
    let kids = build(
        &cr,
        "deadbeef",
        None,
        None,
        test_support::TEST_IMAGE,
        test_support::owner_ref(),
    )
    .unwrap();
    let pod = test_support::pod_spec(&kids.deployment);
    assert!(pod.node_selector.is_none());
    assert!(pod.tolerations.is_none());
    assert!(pod.affinity.is_none());
}

#[test]
fn build_propagates_node_selector_tolerations_and_affinity() {
    use crate::crd::k8s::TolerationSpec;
    let mut cr = test_support::cr("demo", "svc-ns");
    cr.spec.compiler.node_selector.insert("disktype".into(), "ssd".into());
    cr.spec.compiler.tolerations.push(TolerationSpec {
        key: Some("dedicated".into()),
        operator: Some("Equal".into()),
        value: Some("compiler".into()),
        effect: Some("NoSchedule".into()),
        toleration_seconds: None,
    });
    cr.spec.compiler.affinity = Some(serde_json::json!({
        "nodeAffinity": {
            "requiredDuringSchedulingIgnoredDuringExecution": {
                "nodeSelectorTerms": [{
                    "matchExpressions": [{
                        "key": "kubernetes.io/arch",
                        "operator": "In",
                        "values": ["amd64"],
                    }],
                }],
            },
        },
    }));
    let kids = build(
        &cr,
        "deadbeef",
        None,
        None,
        test_support::TEST_IMAGE,
        test_support::owner_ref(),
    )
    .unwrap();
    let pod = test_support::pod_spec(&kids.deployment);
    let ns = pod.node_selector.as_ref().unwrap();
    assert_eq!(ns.get("disktype").map(String::as_str), Some("ssd"));
    let tol = pod.tolerations.as_ref().unwrap();
    assert_eq!(tol.len(), 1);
    assert_eq!(tol[0].key.as_deref(), Some("dedicated"));
    assert_eq!(tol[0].effect.as_deref(), Some("NoSchedule"));
    let aff = pod.affinity.as_ref().unwrap();
    let terms = aff
        .node_affinity
        .as_ref()
        .unwrap()
        .required_during_scheduling_ignored_during_execution
        .as_ref()
        .unwrap();
    assert_eq!(terms.node_selector_terms.len(), 1);
}

#[test]
fn build_surfaces_malformed_affinity_as_json_error() {
    let mut cr = test_support::cr("demo", "svc-ns");
    // numeric where an object is expected fails serde_json::from_value.
    cr.spec.compiler.affinity = Some(serde_json::json!({ "nodeAffinity": 42 }));
    match build(
        &cr,
        "deadbeef",
        None,
        None,
        test_support::TEST_IMAGE,
        test_support::owner_ref(),
    ) {
        Err(crate::error::OperatorError::Json(_)) => {}
        Err(other) => panic!("expected Json error, got {other:?}"),
        Ok(_) => panic!("expected error, got Ok"),
    }
}
