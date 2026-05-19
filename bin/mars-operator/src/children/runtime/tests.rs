#![allow(clippy::unwrap_used)]

use super::*;
use crate::children::test_support;

#[test]
fn build_sets_replicas_and_selector() {
    let cr = test_support::cr("demo", "svc-ns");
    let dep = build(
        &cr,
        "abc123",
        false,
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
fn build_propagates_config_checksum_to_pod_template_annotation() {
    let cr = test_support::cr("demo", "svc-ns");
    let dep = build(
        &cr,
        "deadbeef",
        false,
        test_support::TEST_IMAGE,
        test_support::owner_ref(),
    )
    .unwrap();
    let template = &dep.spec.as_ref().unwrap().template;
    let annotations = template.metadata.as_ref().unwrap().annotations.as_ref().unwrap();
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
        false,
        test_support::TEST_IMAGE,
        test_support::owner_ref(),
    )
    .unwrap();
    let pod = test_support::pod_spec(&dep);
    let volumes = pod.volumes.as_ref().unwrap();
    assert_eq!(volumes.len(), 3);
    assert_eq!(volumes[0].name, "config");
    assert_eq!(volumes[1].name, "cache");
    assert_eq!(volumes[2].name, "fonts");
    assert_eq!(volumes[2].config_map.as_ref().unwrap().name, "custom-fonts");

    let mounts = pod.containers[0].volume_mounts.as_ref().unwrap();
    assert_eq!(mounts.len(), 3);
    assert_eq!(mounts[0].name, "config");
    assert_eq!(mounts[1].name, "cache");
    assert_eq!(mounts[2].name, "fonts");
    assert_eq!(mounts[2].mount_path, "/var/lib/mars/fonts");
    assert_eq!(mounts[2].read_only, Some(true));
}

#[test]
fn build_propagates_scheduling_fields_into_pod_spec() {
    use crate::crd::k8s::TolerationSpec;
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
        false,
        test_support::TEST_IMAGE,
        test_support::owner_ref(),
    )
    .unwrap();
    let pod = test_support::pod_spec(&dep);
    assert_eq!(
        pod.node_selector.as_ref().unwrap().get("zone").map(String::as_str),
        Some("eu-west-1a")
    );
    assert_eq!(pod.tolerations.as_ref().unwrap()[0].key.as_deref(), Some("dedicated"));
    let pref = pod
        .affinity
        .as_ref()
        .unwrap()
        .pod_anti_affinity
        .as_ref()
        .unwrap()
        .preferred_during_scheduling_ignored_during_execution
        .as_ref()
        .unwrap();
    assert_eq!(pref.len(), 1);
    assert_eq!(pref[0].weight, 100);
}
