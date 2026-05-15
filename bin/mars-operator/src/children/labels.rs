//! Per-CR naming and label/selector helpers. The base manifests use a single
//! global selector keyed on `app.kubernetes.io/component`; that collides as
//! soon as a second MarsService lands in the namespace, so the operator keys
//! every child on (instance, component).

use std::collections::BTreeMap;

pub(crate) const COMPONENT_COMPILER: &str = "compiler";
pub(crate) const COMPONENT_RUNTIME: &str = "runtime";
pub(crate) const COMPONENT_BOOTSTRAP: &str = "bootstrap";
pub(crate) const COMPONENT_TEARDOWN: &str = "teardown";

pub(crate) const CONFIG_CHECKSUM_ANNOTATION: &str = "mars.forn.dk/config-checksum";

pub(crate) fn bootstrap_service_account_name(svc: &str) -> String {
    format!("{svc}-bootstrap")
}

pub(crate) fn bootstrap_job_name(svc: &str, hash: &str) -> String {
    format!("{svc}-bootstrap-{hash}")
}

pub(crate) fn teardown_job_name(svc: &str) -> String {
    format!("{svc}-teardown")
}

pub(crate) fn config_map_name(svc: &str) -> String {
    format!("{svc}-config")
}

pub(crate) fn compiler_deployment_name(svc: &str) -> String {
    format!("{svc}-compiler")
}

pub(crate) fn compiler_cache_pvc_name(svc: &str) -> String {
    format!("{svc}-compiler-cache")
}

pub(crate) fn compiler_work_pvc_name(svc: &str) -> String {
    format!("{svc}-compiler-work")
}

pub(crate) fn runtime_deployment_name(svc: &str) -> String {
    format!("{svc}-runtime")
}

pub(crate) fn runtime_service_name(svc: &str) -> String {
    format!("{svc}-runtime")
}

pub(crate) fn artifact_store_pvc_name(svc: &str) -> String {
    format!("{svc}-artifact-store")
}

/// Standard labels applied to every child object. `component` is per-child.
pub(crate) fn labels(instance: &str, component: &str) -> BTreeMap<String, String> {
    [
        ("app.kubernetes.io/name", "mars"),
        ("app.kubernetes.io/instance", instance),
        ("app.kubernetes.io/component", component),
        ("app.kubernetes.io/part-of", "mars"),
        ("app.kubernetes.io/managed-by", "mars-operator"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// Selector pinned to (instance, component) so two MarsServices in the same
/// namespace never claim each other's pods.
pub(crate) fn selector(instance: &str, component: &str) -> BTreeMap<String, String> {
    [
        ("app.kubernetes.io/instance", instance),
        ("app.kubernetes.io/component", component),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
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
}
