#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn child_names_are_instance_scoped() {
    assert_eq!(config_map_name("demo"), "demo-config");
    assert_eq!(compiler_deployment_name("demo"), "demo-compiler");
    assert_eq!(compiler_cache_pvc_name("demo"), "demo-compiler-cache");
    assert_eq!(compiler_work_pvc_name("demo"), "demo-compiler-work");
    assert_eq!(runtime_deployment_name("demo"), "demo-runtime");
    assert_eq!(runtime_service_name("demo"), "demo-runtime");
    assert_eq!(artifact_store_pvc_name("demo"), "demo-artifact-store");
}

#[test]
fn labels_include_required_recommended_set() {
    let l = labels("demo", COMPONENT_COMPILER);
    assert_eq!(l.get("app.kubernetes.io/name").map(String::as_str), Some("mars"));
    assert_eq!(l.get("app.kubernetes.io/instance").map(String::as_str), Some("demo"));
    assert_eq!(
        l.get("app.kubernetes.io/component").map(String::as_str),
        Some(COMPONENT_COMPILER)
    );
    assert_eq!(l.get("app.kubernetes.io/part-of").map(String::as_str), Some("mars"));
    assert_eq!(
        l.get("app.kubernetes.io/managed-by").map(String::as_str),
        Some("mars-operator")
    );
}

#[test]
fn selector_pins_instance_and_component_only() {
    let s = selector("demo", COMPONENT_RUNTIME);
    // wider label sets would let two MarsServices claim each other's
    // pods; the selector must stay narrow.
    assert_eq!(s.len(), 2);
    assert_eq!(s.get("app.kubernetes.io/instance").map(String::as_str), Some("demo"));
    assert_eq!(
        s.get("app.kubernetes.io/component").map(String::as_str),
        Some(COMPONENT_RUNTIME)
    );
}
