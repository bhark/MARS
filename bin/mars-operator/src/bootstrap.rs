//! MarsService bootstrap orchestration: per-CR bootstrap and teardown Jobs.
//!
//! The operator does not talk to postgres directly. It renders a Job whose
//! container image is the same `mars` binary the runtime/compiler use, and
//! whose command is `mars setup --config /etc/mars/mars.yaml` (or
//! `mars teardown ...` on CR delete). The admin DSN and the runtime password
//! are projected into the Job pod via `secretKeyRef` only; the always-on
//! compiler/runtime never sees the admin credential.
//!
//! Job names embed a content hash of the bootstrap-relevant fields so a spec
//! change spawns a new Job and the previous one's outcome stays visible.

use blake3::Hasher;
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    ConfigMapVolumeSource, Container, EnvVar, EnvVarSource, PodSpec, PodTemplateSpec, SecretKeySelector,
    ServiceAccount, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};

use crate::children::compiler::{container_security_context, pod_security_context};
use crate::children::labels::{
    self, COMPONENT_BOOTSTRAP, COMPONENT_TEARDOWN, bootstrap_job_name, bootstrap_service_account_name, config_map_name,
    teardown_job_name,
};
use crate::crd::{EnvVarSpec, MarsService, SecretKeyRef, TeardownPolicy};
use crate::error::{OperatorError, Result};

/// Finalizer added to MarsService when a successful bootstrap Job has run.
/// Removal is gated on the teardown Job completing, so a CR delete cannot
/// orphan the slot / publication / role on the source.
pub(crate) const BOOTSTRAP_FINALIZER: &str = "mars.forn.dk/bootstrap";

/// Inputs the operator needs from the CR + cluster state to render a Job.
pub(crate) struct PlanInputs {
    /// Bootstrap settings extracted from `spec.config.sources[].bootstrap`.
    pub(crate) source_bootstrap: SourceBootstrap,
    /// Resolved Secret holding the runtime role password. Either the BYO
    /// `bootstrap.runtimePasswordSecretRef` or the operator-managed
    /// `<svc>-runtime-credentials` Secret. The bootstrap Job reads it; the
    /// compiler/runtime pods get the same projection as `MARS_RUNTIME_PASSWORD`.
    pub(crate) runtime_password_ref: SecretKeyRef,
    /// Resolved Secret reference holding the admin DSN. Either the BYO
    /// `bootstrap.adminSecretRef` or the operator-managed
    /// `<svc>-bootstrap-admin-credentials` Secret composed from the
    /// component-style `bootstrap.adminCredentialsRef`. Always projected
    /// into the Job via `secretKeyRef`; the DSN never lands on the Job spec.
    pub(crate) admin_dsn_ref: SecretKeyRef,
    /// resourceVersion of the resolved admin Secret; rolls the hash on
    /// rotation (BYO Secret bump or recompose).
    pub(crate) admin_secret_resource_version: String,
    /// resourceVersion of the runtime password secret; rolls the hash on
    /// rotation.
    pub(crate) runtime_secret_resource_version: String,
}

#[derive(Clone)]
pub(crate) struct SourceBootstrap {
    pub(crate) role: String,
    pub(crate) publication: String,
    pub(crate) slot: String,
    pub(crate) schemas: Vec<String>,
}

/// Stable short hash over the inputs that materially change what the
/// bootstrap Job would do. A different hash means a different Job name and
/// therefore a fresh execution.
pub(crate) fn plan_hash(inputs: &PlanInputs) -> String {
    let mut h = Hasher::new();
    h.update(inputs.source_bootstrap.role.as_bytes());
    h.update(b"|");
    h.update(inputs.source_bootstrap.publication.as_bytes());
    h.update(b"|");
    h.update(inputs.source_bootstrap.slot.as_bytes());
    h.update(b"|");
    let mut schemas = inputs.source_bootstrap.schemas.clone();
    schemas.sort();
    for s in &schemas {
        h.update(s.as_bytes());
        h.update(b",");
    }
    h.update(b"|");
    h.update(inputs.admin_dsn_ref.name.as_bytes());
    h.update(b":");
    h.update(inputs.admin_dsn_ref.key.as_bytes());
    h.update(b"|");
    h.update(inputs.admin_secret_resource_version.as_bytes());
    h.update(b"|");
    h.update(inputs.runtime_password_ref.name.as_bytes());
    h.update(b":");
    h.update(inputs.runtime_password_ref.key.as_bytes());
    h.update(b"|");
    h.update(inputs.runtime_secret_resource_version.as_bytes());

    let digest = h.finalize();
    // 10 hex chars is plenty for collision resistance at the scale of one CR
    let hex = digest.to_hex();
    hex.as_str()[..10].to_string()
}

/// ServiceAccount used by both bootstrap and teardown Jobs. No RBAC bound
/// to it - the Job pod talks to postgres, not to the apiserver.
pub(crate) fn render_service_account(svc: &str, ns: &str, owner: OwnerReference) -> ServiceAccount {
    ServiceAccount {
        metadata: ObjectMeta {
            name: Some(bootstrap_service_account_name(svc)),
            namespace: Some(ns.to_string()),
            labels: Some(labels::labels(svc, COMPONENT_BOOTSTRAP)),
            owner_references: Some(vec![owner]),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Render the bootstrap Job. The pod runs `mars setup --config <mounted CM>`
/// with `MARS_ADMIN_DSN` and `MARS_RUNTIME_PASSWORD` from the referenced
/// secrets.
pub(crate) fn render_bootstrap_job(
    cr: &MarsService,
    image: &str,
    inputs: &PlanInputs,
    hash: &str,
    owner: OwnerReference,
) -> Result<Job> {
    let svc = cr_name(cr)?;
    let ns = cr_namespace(cr)?;

    let container = Container {
        name: "bootstrap".into(),
        image: Some(image.to_string()),
        args: Some(vec!["setup".into(), "--config".into(), "/etc/mars/mars.yaml".into()]),
        env: Some(job_env(
            vec![
                secret_env("MARS_ADMIN_DSN", &inputs.admin_dsn_ref),
                secret_env("MARS_RUNTIME_PASSWORD", &inputs.runtime_password_ref),
            ],
            &cr.spec.compiler.env,
        )),
        env_from: Some(crate::children::compiler::env_from(&cr.spec.compiler.env_from)),
        volume_mounts: Some(vec![VolumeMount {
            name: "config".into(),
            mount_path: "/etc/mars/mars.yaml".into(),
            sub_path: Some("mars.yaml".into()),
            read_only: Some(true),
            ..Default::default()
        }]),
        security_context: Some(container_security_context()),
        ..Default::default()
    };

    Ok(render_job(
        &svc,
        &ns,
        &bootstrap_job_name(&svc, hash),
        COMPONENT_BOOTSTRAP,
        container,
        owner,
    ))
}

/// Render the teardown Job. Uses the same image, ServiceAccount, and config
/// mount as the bootstrap Job; differs only in command.
pub(crate) fn render_teardown_job(
    cr: &MarsService,
    image: &str,
    admin_dsn_ref: &SecretKeyRef,
    policy: &TeardownPolicy,
    owner: OwnerReference,
) -> Result<Job> {
    let svc = cr_name(cr)?;
    let ns = cr_namespace(cr)?;

    let mut args = vec!["teardown".into(), "--config".into(), "/etc/mars/mars.yaml".into()];
    if policy.slot {
        args.push("--drop-slot".into());
    }
    if policy.publication {
        args.push("--drop-publication".into());
    }
    if policy.role {
        args.push("--drop-role".into());
    }

    let container = Container {
        name: "teardown".into(),
        image: Some(image.to_string()),
        args: Some(args),
        env: Some(job_env(
            vec![secret_env("MARS_ADMIN_DSN", admin_dsn_ref)],
            &cr.spec.compiler.env,
        )),
        env_from: Some(crate::children::compiler::env_from(&cr.spec.compiler.env_from)),
        volume_mounts: Some(vec![VolumeMount {
            name: "config".into(),
            mount_path: "/etc/mars/mars.yaml".into(),
            sub_path: Some("mars.yaml".into()),
            read_only: Some(true),
            ..Default::default()
        }]),
        security_context: Some(container_security_context()),
        ..Default::default()
    };

    Ok(render_job(
        &svc,
        &ns,
        &teardown_job_name(&svc),
        COMPONENT_TEARDOWN,
        container,
        owner,
    ))
}

fn render_job(
    svc: &str,
    ns: &str,
    job_name: &str,
    component: &str,
    container: Container,
    owner: OwnerReference,
) -> Job {
    let labels_map = labels::labels(svc, component);
    let mut selector = labels::selector(svc, component);
    // also include the job-name label k8s sets automatically so existing
    // Pods/Jobs aren't accidentally claimed across upgrades.
    selector.insert("job-name".into(), job_name.into());

    Job {
        metadata: ObjectMeta {
            name: Some(job_name.to_string()),
            namespace: Some(ns.to_string()),
            labels: Some(labels_map.clone()),
            owner_references: Some(vec![owner]),
            ..Default::default()
        },
        spec: Some(JobSpec {
            backoff_limit: Some(3),
            // ~24h: keeps the failed/succeeded Job around long enough for
            // operators to inspect, gone before the next CR generation churn
            // accumulates clutter.
            ttl_seconds_after_finished: Some(86_400),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels_map),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    restart_policy: Some("Never".into()),
                    service_account_name: Some(bootstrap_service_account_name(svc)),
                    security_context: Some(pod_security_context()),
                    containers: vec![container],
                    volumes: Some(vec![Volume {
                        name: "config".into(),
                        config_map: Some(ConfigMapVolumeSource {
                            name: config_map_name(svc),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        status: None,
    }
}

/// Merge the operator-projected secret env (admin DSN / runtime password)
/// with the compiler's user-supplied `env`. The Job mounts the compiler's
/// mars.yaml ConfigMap, so it needs the same environment to resolve every
/// `${...}` placeholder at config-load time - notably the source `dsn`.
/// The secret env wins on a name collision.
fn job_env(secret_envs: Vec<EnvVar>, compiler_env: &[EnvVarSpec]) -> Vec<EnvVar> {
    let mut out = secret_envs;
    for e in crate::children::compiler::env_vars(compiler_env) {
        if !out.iter().any(|x| x.name == e.name) {
            out.push(e);
        }
    }
    out
}

fn secret_env(name: &str, sref: &SecretKeyRef) -> EnvVar {
    EnvVar {
        name: name.into(),
        value_from: Some(EnvVarSource {
            secret_key_ref: Some(SecretKeySelector {
                name: sref.name.clone(),
                key: sref.key.clone(),
                optional: Some(false),
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Pull a postgis source's `bootstrap` + the publication/slot names out of an
/// opaque `spec.config` JSON value. Picks the first `sources[]` entry whose
/// `bootstrap` block is set. Returns `None` when no bootstrap-bearing source
/// is declared (the operator treats this as "feature off").
pub(crate) fn extract_source_bootstrap(config: &serde_json::Value) -> Option<SourceBootstrap> {
    let source = resolve_bootstrap_source(config)?;
    let bs = source.get("bootstrap")?;
    let cf = source.get("change_feed")?;

    let role = bs.get("role")?.as_str()?.to_string();
    let publication = cf.get("publication")?.as_str()?.to_string();
    let slot = cf.get("slot")?.as_str()?.to_string();
    let schemas = bs
        .get("schemas")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect::<Vec<_>>();
    if schemas.is_empty() {
        return None;
    }
    Some(SourceBootstrap {
        role,
        publication,
        slot,
        schemas,
    })
}

fn resolve_bootstrap_source(config: &serde_json::Value) -> Option<&serde_json::Value> {
    config
        .get("sources")?
        .as_array()?
        .iter()
        .find(|s| s.get("bootstrap").is_some())
}

/// Read the `dsn` string off the bootstrap-bearing source so the operator can
/// derive host/port/database fallbacks for component-style admin credentials.
/// Returns an empty string when no bootstrap source is configured; the
/// component-style composer treats empty as "no fallbacks available".
pub(crate) fn source_dsn_for_fallback(config: &serde_json::Value) -> String {
    resolve_bootstrap_source(config)
        .and_then(|s| s.get("dsn"))
        .and_then(|d| d.as_str())
        .unwrap_or_default()
        .to_string()
}

fn cr_name(cr: &MarsService) -> Result<String> {
    cr.metadata
        .name
        .clone()
        .ok_or_else(|| OperatorError::MissingField("metadata.name".into()))
}

fn cr_namespace(cr: &MarsService) -> Result<String> {
    cr.metadata
        .namespace
        .clone()
        .ok_or_else(|| OperatorError::MissingField("metadata.namespace".into()))
}

#[cfg(test)]
mod tests;
