//! MarsService bootstrap orchestration: per-CR bootstrap and teardown Jobs.
//!
//! The operator does not talk to postgres directly. It renders a Job whose
//! container image is the same `mars` binary the runtime/compiler use, and
//! whose command is `mars setup --config /etc/mars/mars.yaml` (or
//! `mars teardown ...` on CR delete). The admin DSN and the runtime password
//! are mounted from secrets only into the Job pod; the always-on
//! compiler/runtime never sees the admin secret.
//!
//! Job names embed a content hash of the bootstrap-relevant fields so a spec
//! change spawns a new Job and the previous one's outcome stays visible.

use blake3::Hasher;
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    ConfigMapVolumeSource, Container, EnvVar, EnvVarSource, PodSpec, PodTemplateSpec, SecretKeySelector, ServiceAccount,
    Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};

use crate::children::compiler::{container_security_context, pod_security_context};
use crate::children::labels::{
    self, COMPONENT_BOOTSTRAP, COMPONENT_TEARDOWN, bootstrap_job_name, bootstrap_service_account_name,
    config_map_name, teardown_job_name,
};
use crate::crd::{BootstrapSpec, MarsService, SecretKeyRef, TeardownPolicy};
use crate::error::{OperatorError, Result};

/// Finalizer added to MarsService when a successful bootstrap Job has run.
/// Removal is gated on the teardown Job completing, so a CR delete cannot
/// orphan the slot / publication / role on the source.
pub(crate) const BOOTSTRAP_FINALIZER: &str = "mars.forn.dk/bootstrap";

/// Inputs the operator needs from the CR + cluster state to render a Job.
pub(crate) struct PlanInputs<'a> {
    pub(crate) bootstrap: &'a BootstrapSpec,
    /// Bootstrap settings extracted from `spec.config.source.bootstrap`.
    pub(crate) source_bootstrap: SourceBootstrap,
    /// resourceVersion of the admin secret; rolls the hash on rotation.
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
pub(crate) fn plan_hash(inputs: &PlanInputs<'_>) -> String {
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
    if let Some(r) = &inputs.bootstrap.admin_secret_ref {
        h.update(r.name.as_bytes());
        h.update(b":");
        h.update(r.key.as_bytes());
    }
    h.update(b"|");
    h.update(inputs.admin_secret_resource_version.as_bytes());
    h.update(b"|");
    if let Some(r) = &inputs.bootstrap.runtime_password_secret_ref {
        h.update(r.name.as_bytes());
        h.update(b":");
        h.update(r.key.as_bytes());
    }
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
    inputs: &PlanInputs<'_>,
    hash: &str,
    owner: OwnerReference,
) -> Result<Job> {
    let svc = cr_name(cr)?;
    let ns = cr_namespace(cr)?;
    let admin = inputs
        .bootstrap
        .admin_secret_ref
        .as_ref()
        .ok_or_else(|| OperatorError::ConfigInvalid("bootstrap.adminSecretRef is required".into()))?;
    let runtime = inputs
        .bootstrap
        .runtime_password_secret_ref
        .as_ref()
        .ok_or_else(|| OperatorError::ConfigInvalid(
            "bootstrap.runtimePasswordSecretRef is required".into(),
        ))?;

    let container = Container {
        name: "bootstrap".into(),
        image: Some(image.to_string()),
        args: Some(vec![
            "setup".into(),
            "--config".into(),
            "/etc/mars/mars.yaml".into(),
        ]),
        env: Some(vec![
            secret_env("MARS_ADMIN_DSN", admin),
            secret_env("MARS_RUNTIME_PASSWORD", runtime),
        ]),
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
    bootstrap: &BootstrapSpec,
    policy: &TeardownPolicy,
    owner: OwnerReference,
) -> Result<Job> {
    let svc = cr_name(cr)?;
    let ns = cr_namespace(cr)?;
    let admin = bootstrap
        .admin_secret_ref
        .as_ref()
        .ok_or_else(|| OperatorError::ConfigInvalid("bootstrap.adminSecretRef is required".into()))?;

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
        env: Some(vec![secret_env("MARS_ADMIN_DSN", admin)]),
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

/// Pull `source.bootstrap` + the publication/slot names out of an opaque
/// `spec.config` JSON value. Returns `None` when the config does not declare
/// a bootstrap block (the operator treats this as "feature off").
pub(crate) fn extract_source_bootstrap(config: &serde_json::Value) -> Option<SourceBootstrap> {
    let source = config.get("source")?;
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::children::test_support;
    use crate::crd::TeardownPolicy;

    fn admin() -> SecretKeyRef {
        SecretKeyRef {
            name: "admin-secret".into(),
            key: "dsn".into(),
        }
    }

    fn runtime() -> SecretKeyRef {
        SecretKeyRef {
            name: "runtime-secret".into(),
            key: "password".into(),
        }
    }

    fn bs_spec() -> BootstrapSpec {
        BootstrapSpec {
            enabled: true,
            admin_secret_ref: Some(admin()),
            runtime_password_secret_ref: Some(runtime()),
            teardown_on_delete: TeardownPolicy::default(),
        }
    }

    fn source_bootstrap() -> SourceBootstrap {
        SourceBootstrap {
            role: "mars_replicator".into(),
            publication: "mars_pub".into(),
            slot: "mars_slot".into(),
            schemas: vec!["public".into(), "geo".into()],
        }
    }

    fn inputs(bs: &BootstrapSpec) -> PlanInputs<'_> {
        PlanInputs {
            bootstrap: bs,
            source_bootstrap: source_bootstrap(),
            admin_secret_resource_version: "100".into(),
            runtime_secret_resource_version: "200".into(),
        }
    }

    #[test]
    fn plan_hash_is_stable_for_identical_inputs() {
        let bs = bs_spec();
        let h1 = plan_hash(&inputs(&bs));
        let h2 = plan_hash(&inputs(&bs));
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 10);
    }

    #[test]
    fn plan_hash_changes_on_schema_change() {
        let bs = bs_spec();
        let h1 = plan_hash(&inputs(&bs));
        let mut other = inputs(&bs);
        other.source_bootstrap.schemas = vec!["public".into(), "extra".into()];
        let h2 = plan_hash(&other);
        assert_ne!(h1, h2);
    }

    #[test]
    fn plan_hash_changes_on_admin_secret_rotation() {
        let bs = bs_spec();
        let h1 = plan_hash(&inputs(&bs));
        let mut other = inputs(&bs);
        other.admin_secret_resource_version = "101".into();
        let h2 = plan_hash(&other);
        assert_ne!(h1, h2);
    }

    #[test]
    fn plan_hash_independent_of_schema_order() {
        let bs = bs_spec();
        let h1 = plan_hash(&inputs(&bs));
        let mut other = inputs(&bs);
        other.source_bootstrap.schemas = vec!["geo".into(), "public".into()];
        let h2 = plan_hash(&other);
        assert_eq!(h1, h2);
    }

    #[test]
    fn render_bootstrap_job_has_two_secret_env_vars() {
        let cr = test_support::cr("demo", "svc-ns");
        let bs = bs_spec();
        let job = render_bootstrap_job(
            &cr,
            test_support::TEST_IMAGE,
            &inputs(&bs),
            "abcdef0123",
            test_support::owner_ref(),
        )
        .unwrap();
        assert_eq!(job.metadata.name.as_deref(), Some("demo-bootstrap-abcdef0123"));
        let pod = job.spec.unwrap().template.spec.unwrap();
        let envs = pod.containers[0].env.as_ref().unwrap();
        assert!(envs.iter().any(|e| e.name == "MARS_ADMIN_DSN"));
        assert!(envs.iter().any(|e| e.name == "MARS_RUNTIME_PASSWORD"));
        assert_eq!(pod.restart_policy.as_deref(), Some("Never"));
        assert_eq!(pod.service_account_name.as_deref(), Some("demo-bootstrap"));
    }

    #[test]
    fn render_teardown_job_omits_drop_flags_when_disabled() {
        let cr = test_support::cr("demo", "svc-ns");
        let bs = bs_spec();
        let policy = TeardownPolicy {
            slot: true,
            publication: false,
            role: false,
        };
        let job = render_teardown_job(
            &cr,
            test_support::TEST_IMAGE,
            &bs,
            &policy,
            test_support::owner_ref(),
        )
        .unwrap();
        let args = job.spec.unwrap().template.spec.unwrap().containers[0]
            .args
            .clone()
            .unwrap();
        assert!(args.iter().any(|a| a == "--drop-slot"));
        assert!(!args.iter().any(|a| a == "--drop-publication"));
        assert!(!args.iter().any(|a| a == "--drop-role"));
    }

    #[test]
    fn render_teardown_job_omits_runtime_password_env() {
        let cr = test_support::cr("demo", "svc-ns");
        let bs = bs_spec();
        let job = render_teardown_job(
            &cr,
            test_support::TEST_IMAGE,
            &bs,
            &TeardownPolicy::default(),
            test_support::owner_ref(),
        )
        .unwrap();
        let envs = job.spec.unwrap().template.spec.unwrap().containers[0]
            .env
            .clone()
            .unwrap();
        assert!(envs.iter().any(|e| e.name == "MARS_ADMIN_DSN"));
        assert!(!envs.iter().any(|e| e.name == "MARS_RUNTIME_PASSWORD"));
    }

    #[test]
    fn extract_source_bootstrap_returns_none_without_bootstrap_block() {
        let config = serde_json::json!({
            "source": { "change_feed": { "publication": "p", "slot": "s" } }
        });
        assert!(extract_source_bootstrap(&config).is_none());
    }

    #[test]
    fn extract_source_bootstrap_pulls_names_and_schemas() {
        let config = serde_json::json!({
            "source": {
                "change_feed": { "publication": "mars_pub", "slot": "mars_slot" },
                "bootstrap": { "role": "mars_replicator", "schemas": ["a", "b"] }
            }
        });
        let bs = extract_source_bootstrap(&config).unwrap();
        assert_eq!(bs.role, "mars_replicator");
        assert_eq!(bs.publication, "mars_pub");
        assert_eq!(bs.slot, "mars_slot");
        assert_eq!(bs.schemas, vec!["a".to_string(), "b".to_string()]);
    }
}
