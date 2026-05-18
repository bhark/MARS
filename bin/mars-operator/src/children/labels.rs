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

/// Fixed key the operator-managed runtime-credentials Secret stores the
/// password under. Documented in the CRD doc and `docs/postgres-setup.md`.
pub(crate) const RUNTIME_PASSWORD_KEY: &str = "password";

/// Env var the operator projects into compiler/runtime pods so users can
/// template the password into `spec.config.source.dsn` via `${MARS_RUNTIME_PASSWORD}`.
pub(crate) const RUNTIME_PASSWORD_ENV: &str = "MARS_RUNTIME_PASSWORD";

/// Fixed key the operator-managed bootstrap-admin-credentials Secret stores
/// the composed admin DSN under.
pub(crate) const BOOTSTRAP_ADMIN_DSN_KEY: &str = "dsn";

/// Component label for the operator-managed Secret holding the composed
/// admin DSN (component-style `adminCredentialsRef` branch).
pub(crate) const COMPONENT_BOOTSTRAP_ADMIN_CREDENTIALS: &str = "bootstrap-admin-credentials";

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

pub(crate) fn runtime_pdb_name(svc: &str) -> String {
    format!("{svc}-runtime")
}

pub(crate) fn artifact_store_pvc_name(svc: &str) -> String {
    format!("{svc}-artifact-store")
}

/// Name of the operator-managed Secret carrying the generated runtime role
/// password when `bootstrap.runtimePasswordSecretRef` is not set.
pub(crate) fn runtime_credentials_secret_name(svc: &str) -> String {
    format!("{svc}-runtime-credentials")
}

/// Name of the operator-managed Secret carrying the composed admin DSN when
/// `bootstrap.adminCredentialsRef` is used.
pub(crate) fn bootstrap_admin_credentials_secret_name(svc: &str) -> String {
    format!("{svc}-bootstrap-admin-credentials")
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
mod tests;
