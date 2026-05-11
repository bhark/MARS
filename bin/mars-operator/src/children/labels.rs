//! Per-CR naming and label/selector helpers. The base manifests use a single
//! global selector keyed on `app.kubernetes.io/component`; that collides as
//! soon as a second MarsService lands in the namespace, so the operator keys
//! every child on (instance, component).

use std::collections::BTreeMap;

pub(crate) const COMPONENT_COMPILER: &str = "compiler";
pub(crate) const COMPONENT_RUNTIME: &str = "runtime";

pub(crate) const CONFIG_CHECKSUM_ANNOTATION: &str = "mars.forn.dk/config-checksum";

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
