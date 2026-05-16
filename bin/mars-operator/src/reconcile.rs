//! Reconcile loop: validate spec, render children, server-side apply,
//! refresh status. Pure of side-effects beyond the kube client and the
//! metrics facade.

use std::sync::Arc;
use std::time::{Duration, Instant};

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Secret, Service, ServiceAccount};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::Resource;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::Action;
use serde_json::json;
use tracing::{error, info, warn};

use crate::bootstrap::{self, BOOTSTRAP_FINALIZER, PlanInputs};
use crate::children::labels::{self, artifact_store_pvc_name};
use crate::children::pvc::{self, PvcSpec};
use crate::children::{compiler, configmap, runtime, service};
use crate::crd::{ArtifactStoreSpec, BootstrapSpec, MarsService, SecretKeyRef};
use crate::error::{OperatorError, Result};
use crate::metrics::Metrics;
use crate::status::{self, BootstrapReason, BootstrapStatus, StatusInputs};

pub(crate) struct Ctx {
    pub(crate) client: kube::Client,
    pub(crate) field_manager: String,
    pub(crate) metrics: Metrics,
    /// `repo:version` for the runtime/compiler containers. Built once at
    /// startup from CLI/env + operator's own CARGO_PKG_VERSION; identical
    /// for every reconcile.
    pub(crate) runtime_image: String,
}

pub(crate) async fn reconcile(cr: Arc<MarsService>, ctx: Arc<Ctx>) -> std::result::Result<Action, OperatorError> {
    let start = Instant::now();
    match reconcile_inner(cr, ctx.clone()).await {
        Ok(action) => {
            ctx.metrics.record("ok", start.elapsed());
            Ok(action)
        }
        Err(e) => {
            error!(error = %e, "reconcile failed");
            ctx.metrics.record("error", start.elapsed());
            ctx.metrics.record_error(error_kind(&e));
            Err(e)
        }
    }
}

fn error_kind(e: &OperatorError) -> &'static str {
    match e {
        OperatorError::Kube(_) => "kube",
        OperatorError::ConfigInvalid(_) => "config_invalid",
        OperatorError::MarsConfig(_) => "mars_config",
        OperatorError::Yaml(_) => "yaml",
        OperatorError::Json(_) => "json",
        OperatorError::MissingField(_) => "missing_field",
    }
}

async fn reconcile_inner(cr: Arc<MarsService>, ctx: Arc<Ctx>) -> Result<Action> {
    let svc_name = cr
        .metadata
        .name
        .clone()
        .ok_or_else(|| OperatorError::MissingField("metadata.name".into()))?;
    let ns = cr
        .metadata
        .namespace
        .clone()
        .ok_or_else(|| OperatorError::MissingField("metadata.namespace".into()))?;
    let generation = cr.metadata.generation.unwrap_or(0);

    info!(svc = %svc_name, ns = %ns, gen = generation, "reconciling MarsService");

    let owner_ref = owner_reference(&cr)?;

    // deletion handling runs before anything else: we must run a teardown
    // Job (if needed) and remove the finalizer before kube cascade-deletes
    // the children. nothing else can succeed once deletionTimestamp is set.
    if cr.metadata.deletion_timestamp.is_some() {
        return reconcile_deletion(cr.clone(), &ctx, &svc_name, &ns).await;
    }

    let (config_valid, config_message) = match crate::config::validate(&cr.spec.config) {
        Ok(()) => (true, "spec.config validated".to_string()),
        Err(e) => (false, e.to_string()),
    };

    if !config_valid {
        warn!(svc = %svc_name, "spec.config invalid: {config_message}");
        let status_body = status::compute(StatusInputs {
            observed_generation: generation,
            config_valid: false,
            config_message: &config_message,
            children_applied: false,
            children_message: "skipped: config invalid",
            compiler_ready: false,
            runtime_ready: false,
            degraded: None,
            bootstrap: None,
            runtime_credentials_secret: None,
        });
        patch_status(&ctx, &svc_name, &ns, status_body).await?;
        return Ok(Action::requeue(Duration::from_secs(30)));
    }

    let fs_store = detect_fs_store(&cr);

    if let Some(reason) = artifact_store_guard(&cr, fs_store.as_ref()) {
        warn!(svc = %svc_name, "degraded: {reason}");
        let status_body = status::compute(StatusInputs {
            observed_generation: generation,
            config_valid: true,
            config_message: &config_message,
            children_applied: false,
            children_message: "skipped: degraded",
            compiler_ready: false,
            runtime_ready: false,
            degraded: Some(&reason),
            bootstrap: None,
            runtime_credentials_secret: None,
        });
        patch_status(&ctx, &svc_name, &ns, status_body).await?;
        return Ok(Action::requeue(Duration::from_secs(30)));
    }

    // configmap must exist before the bootstrap Job pod can mount it. PVCs
    // come later because the bootstrap Job does not need them.
    let (cm, checksum) = configmap::build(&cr, owner_ref.clone())?;
    apply_configmap(&ctx, &ns, &cm).await?;

    let bootstrap_outcome = reconcile_bootstrap(&ctx, &cr, &svc_name, &ns, owner_ref.clone()).await?;
    let runtime_credentials_secret = bootstrap_outcome.runtime_password_ref.as_ref().map(|r| r.name.as_str());
    if !bootstrap_outcome.proceed {
        let status_body = status::compute(StatusInputs {
            observed_generation: generation,
            config_valid: true,
            config_message: &config_message,
            children_applied: false,
            children_message: "skipped: bootstrap not ready",
            compiler_ready: false,
            runtime_ready: false,
            degraded: None,
            bootstrap: Some(bootstrap_outcome.status),
            runtime_credentials_secret,
        });
        patch_status(&ctx, &svc_name, &ns, status_body).await?;
        return Ok(Action::requeue(bootstrap_outcome.requeue));
    }

    if let Some(art) = &fs_store {
        let art_pvc = pvc::build(
            PvcSpec {
                name: &artifact_store_pvc_name(&svc_name),
                namespace: Some(&ns),
                labels: crate::children::labels::labels(&svc_name, "artifact-store"),
                size: &art.size,
                storage_class: &art.storage_class,
                access_modes: &art.access_modes,
            },
            owner_ref.clone(),
        );
        apply_pvc(&ctx, &ns, &art_pvc).await?;
    }

    let runtime_password_ref = bootstrap_outcome.runtime_password_ref.as_ref();
    let compiler_children = compiler::build(
        &cr,
        &checksum,
        fs_store.as_ref(),
        runtime_password_ref,
        &ctx.runtime_image,
        owner_ref.clone(),
    )?;
    apply_pvc(&ctx, &ns, &compiler_children.cache_pvc).await?;
    apply_pvc(&ctx, &ns, &compiler_children.work_pvc).await?;
    apply_deployment(&ctx, &ns, &compiler_children.deployment).await?;

    let runtime_deployment = runtime::build(
        &cr,
        &checksum,
        fs_store.as_ref(),
        runtime_password_ref,
        &ctx.runtime_image,
        owner_ref.clone(),
    )?;
    apply_deployment(&ctx, &ns, &runtime_deployment).await?;

    let runtime_service = service::build(&cr, owner_ref)?;
    apply_service(&ctx, &ns, &runtime_service).await?;

    // re-read deployments for readiness
    let dep_api: Api<Deployment> = Api::namespaced(ctx.client.clone(), &ns);
    let compiler_dep = dep_api
        .get_opt(&crate::children::labels::compiler_deployment_name(&svc_name))
        .await?;
    let runtime_dep = dep_api
        .get_opt(&crate::children::labels::runtime_deployment_name(&svc_name))
        .await?;

    let compiler_ready = compiler_dep.as_ref().map(status::deployment_ready).unwrap_or(false);
    let runtime_ready = runtime_dep.as_ref().map(status::deployment_ready).unwrap_or(false);

    let status_body = status::compute(StatusInputs {
        observed_generation: generation,
        config_valid: true,
        config_message: &config_message,
        children_applied: true,
        children_message: "all children applied",
        compiler_ready,
        runtime_ready,
        degraded: None,
        bootstrap: Some(bootstrap_outcome.status),
        runtime_credentials_secret,
    });
    patch_status(&ctx, &svc_name, &ns, status_body).await?;

    Ok(Action::requeue(Duration::from_secs(30)))
}

/// Outcome of the bootstrap reconciliation step. `proceed = false` halts the
/// reconcile here and surfaces the condition to the user without applying
/// compiler/runtime children.
struct BootstrapOutcome {
    proceed: bool,
    status: BootstrapStatus<'static>,
    requeue: Duration,
    /// Resolved Secret holding the runtime role password. Always Some when
    /// `spec.bootstrap` is declared (BYO or operator-managed); None means
    /// the legacy "no bootstrap" path and no MARS_RUNTIME_PASSWORD env is
    /// projected into compiler/runtime pods.
    runtime_password_ref: Option<SecretKeyRef>,
}

async fn reconcile_bootstrap(
    ctx: &Ctx,
    cr: &MarsService,
    svc_name: &str,
    ns: &str,
    owner: OwnerReference,
) -> Result<BootstrapOutcome> {
    let bs_spec = match cr.spec.bootstrap.as_ref() {
        Some(b) => b,
        None => {
            // no bootstrap declared: legacy path. emit no condition (Some
            // would be misleading - we have nothing to report) and proceed.
            return Ok(BootstrapOutcome {
                proceed: true,
                status: BootstrapStatus {
                    ready: true,
                    reason: BootstrapReason::ManualVerified,
                    message: "no spec.bootstrap declared",
                },
                requeue: Duration::from_secs(30),
                runtime_password_ref: None,
            });
        }
    };
    let source_bs = match bootstrap::extract_source_bootstrap(&cr.spec.config) {
        Some(s) => s,
        None => {
            return Ok(BootstrapOutcome {
                proceed: false,
                status: BootstrapStatus {
                    ready: false,
                    reason: BootstrapReason::ManualSetupIncomplete,
                    message: "spec.bootstrap is set but spec.config.source.bootstrap is missing",
                },
                requeue: Duration::from_secs(30),
                runtime_password_ref: None,
            });
        }
    };

    if !bs_spec.enabled {
        // manual mode. trust the user; the runtime/compiler will surface
        // any actual prerequisite mismatch via their own startup logs.
        return Ok(BootstrapOutcome {
            proceed: true,
            status: BootstrapStatus {
                ready: true,
                reason: BootstrapReason::ManualVerified,
                message: "bootstrap.enabled=false; assuming manual setup is in place",
            },
            requeue: Duration::from_secs(60),
            runtime_password_ref: bs_spec.runtime_password_secret_ref.clone(),
        });
    }

    // resolve admin + runtime secret resourceVersions so the plan hash rolls
    // when either secret is rotated.
    let secret_api: Api<Secret> = Api::namespaced(ctx.client.clone(), ns);
    let resolved_admin = resolve_admin_dsn(&secret_api, bs_spec, &cr.spec.config).await?;
    let runtime_password_ref = ensure_runtime_password_secret(ctx, svc_name, ns, bs_spec, owner.clone()).await?;
    let runtime_rv = secret_api
        .get_opt(&runtime_password_ref.name)
        .await?
        .and_then(|s| s.metadata.resource_version.clone())
        .unwrap_or_default();

    let inputs = PlanInputs {
        source_bootstrap: source_bs,
        runtime_password_ref: runtime_password_ref.clone(),
        admin_dsn: resolved_admin.dsn,
        admin_secret_resource_version: resolved_admin.source_secret_resource_version,
        runtime_secret_resource_version: runtime_rv,
    };
    let hash = bootstrap::plan_hash(&inputs);

    // ServiceAccount for the Job. SSA so re-applies are no-ops.
    let sa = bootstrap::render_service_account(svc_name, ns, owner.clone());
    apply_service_account(ctx, ns, &sa).await?;

    // ensure or observe the Job for this hash.
    let job_name = crate::children::labels::bootstrap_job_name(svc_name, &hash);
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), ns);
    let existing = job_api.get_opt(&job_name).await?;

    let job = bootstrap::render_bootstrap_job(cr, &ctx.runtime_image, &inputs, &hash, owner)?;
    let Some(existing) = existing else {
        job_api
            .patch(
                &job_name,
                &PatchParams::apply(&ctx.field_manager).force(),
                &Patch::Apply(&job),
            )
            .await?;
        return Ok(BootstrapOutcome {
            proceed: false,
            status: BootstrapStatus {
                ready: false,
                reason: BootstrapReason::InProgress,
                message: "bootstrap Job created; waiting for completion",
            },
            requeue: Duration::from_secs(10),
            runtime_password_ref: Some(runtime_password_ref),
        });
    };
    let st = existing.status.as_ref();
    let succeeded = st.and_then(|s| s.succeeded).unwrap_or(0);
    let failed = st.and_then(|s| s.failed).unwrap_or(0);

    if succeeded >= 1 {
        // mark the finalizer so a future delete runs teardown.
        ensure_finalizer(ctx, cr, svc_name, ns).await?;
        Ok(BootstrapOutcome {
            proceed: true,
            status: BootstrapStatus {
                ready: true,
                reason: BootstrapReason::Ready,
                message: "bootstrap Job succeeded",
            },
            requeue: Duration::from_secs(60),
            runtime_password_ref: Some(runtime_password_ref),
        })
    } else if failed >= 3 {
        Ok(BootstrapOutcome {
            proceed: false,
            status: BootstrapStatus {
                ready: false,
                reason: BootstrapReason::Failed,
                message: "bootstrap Job failed; inspect Job pods for logs",
            },
            requeue: Duration::from_secs(60),
            runtime_password_ref: Some(runtime_password_ref),
        })
    } else {
        Ok(BootstrapOutcome {
            proceed: false,
            status: BootstrapStatus {
                ready: false,
                reason: BootstrapReason::InProgress,
                message: "bootstrap Job in progress",
            },
            requeue: Duration::from_secs(10),
            runtime_password_ref: Some(runtime_password_ref),
        })
    }
}

/// Outcome of admin-DSN resolution: how the Job will receive the DSN plus the
/// resourceVersion of the underlying user Secret (drives plan_hash rotation).
struct ResolvedAdminDsn {
    dsn: bootstrap::AdminDsn,
    source_secret_resource_version: String,
}

/// Validate `bootstrap.adminSecretRef` vs `bootstrap.adminCredentialsRef` (exactly
/// one is required when enabled), then resolve the result into an `AdminDsn`.
/// The component-style branch reads the source Secret and composes the DSN by
/// combining its keys with host/port/database fallbacks parsed out of the
/// bootstrap-bearing `spec.config.sources[].dsn`.
async fn resolve_admin_dsn(
    secret_api: &Api<Secret>,
    bs_spec: &BootstrapSpec,
    config: &serde_json::Value,
) -> Result<ResolvedAdminDsn> {
    match (&bs_spec.admin_secret_ref, &bs_spec.admin_credentials_ref) {
        (Some(_), Some(_)) => Err(OperatorError::ConfigInvalid(
            "bootstrap.adminSecretRef and bootstrap.adminCredentialsRef are mutually exclusive".into(),
        )),
        (None, None) => Err(OperatorError::ConfigInvalid(
            "bootstrap.adminSecretRef or bootstrap.adminCredentialsRef is required when bootstrap.enabled".into(),
        )),
        (Some(r), None) => {
            let rv = secret_api
                .get_opt(&r.name)
                .await?
                .and_then(|s| s.metadata.resource_version.clone())
                .unwrap_or_default();
            Ok(ResolvedAdminDsn {
                dsn: bootstrap::AdminDsn::Secret(r.clone()),
                source_secret_resource_version: rv,
            })
        }
        (None, Some(creds)) => {
            let secret = secret_api.get_opt(&creds.secret_name).await?.ok_or_else(|| {
                OperatorError::ConfigInvalid(format!(
                    "bootstrap.adminCredentialsRef.secretName='{}' not found in namespace",
                    creds.secret_name
                ))
            })?;
            let rv = secret.metadata.resource_version.clone().unwrap_or_default();
            let data: std::collections::BTreeMap<String, Vec<u8>> = secret
                .data
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| (k, v.0))
                .collect();
            let fallback_dsn_src = bootstrap::source_dsn_for_fallback(config);
            let fallback = crate::dsn::parse_dsn_components(&fallback_dsn_src);
            let composed = crate::dsn::compose_admin_dsn(creds, &data, &fallback)
                .map_err(|e| OperatorError::ConfigInvalid(format!("compose admin DSN: {e}")))?;
            Ok(ResolvedAdminDsn {
                dsn: bootstrap::AdminDsn::Literal(composed),
                source_secret_resource_version: rv,
            })
        }
    }
}

/// Resolve the runtime password Secret reference. With a BYO
/// `runtimePasswordSecretRef` the user owns rotation entirely. Without one,
/// the operator generates a cryptographically random password on first
/// reconcile and persists it as `<svc>-runtime-credentials` (key `password`)
/// with an owner reference back to the MarsService. Subsequent reconciles
/// reuse the existing Secret; we never rotate in-place.
async fn ensure_runtime_password_secret(
    ctx: &Ctx,
    svc_name: &str,
    ns: &str,
    bs_spec: &BootstrapSpec,
    owner: OwnerReference,
) -> Result<SecretKeyRef> {
    if let Some(byo) = &bs_spec.runtime_password_secret_ref {
        return Ok(byo.clone());
    }
    let name = labels::runtime_credentials_secret_name(svc_name);
    let key = labels::RUNTIME_PASSWORD_KEY.to_string();
    let api: Api<Secret> = Api::namespaced(ctx.client.clone(), ns);

    if api.get_opt(&name).await?.is_some() {
        return Ok(SecretKeyRef { name, key });
    }

    let password = generate_runtime_password();
    let secret = build_runtime_credentials_secret(&name, ns, svc_name, &password, owner);
    api.patch(
        &name,
        &PatchParams::apply(&ctx.field_manager).force(),
        &Patch::Apply(&secret),
    )
    .await?;
    info!(svc = %svc_name, secret = %name, "generated operator-managed runtime password");
    Ok(SecretKeyRef { name, key })
}

/// 32 chars of [A-Za-z0-9], ~190 bits of entropy. Alphanumeric to keep the
/// password embeddable in a libpq URI DSN without URL-encoding.
fn generate_runtime_password() -> String {
    use rand::RngExt;
    use rand::distr::Alphanumeric;
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

fn build_runtime_credentials_secret(name: &str, ns: &str, svc: &str, password: &str, owner: OwnerReference) -> Secret {
    use k8s_openapi::ByteString;
    let mut data = std::collections::BTreeMap::new();
    data.insert(
        labels::RUNTIME_PASSWORD_KEY.into(),
        ByteString(password.as_bytes().to_vec()),
    );
    Secret {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(name.into()),
            namespace: Some(ns.into()),
            labels: Some(labels::labels(svc, "runtime-credentials")),
            owner_references: Some(vec![owner]),
            ..Default::default()
        },
        data: Some(data),
        type_: Some("Opaque".into()),
        ..Default::default()
    }
}

async fn reconcile_deletion(
    cr: Arc<MarsService>,
    ctx: &Ctx,
    svc_name: &str,
    ns: &str,
) -> std::result::Result<Action, OperatorError> {
    let has_finalizer = cr
        .metadata
        .finalizers
        .as_ref()
        .map(|f| f.iter().any(|s| s == BOOTSTRAP_FINALIZER))
        .unwrap_or(false);
    if !has_finalizer {
        // nothing to clean up; let the cascade complete.
        return Ok(Action::await_change());
    }
    let Some(bs_spec) = cr.spec.bootstrap.as_ref() else {
        // finalizer present but spec.bootstrap removed: just drop the
        // finalizer; nothing to roll back.
        remove_finalizer(ctx, cr.as_ref(), svc_name, ns).await?;
        return Ok(Action::await_change());
    };
    let policy = &bs_spec.teardown_on_delete;
    let nothing_to_drop = !policy.slot && !policy.publication && !policy.role;
    let has_admin = bs_spec.admin_secret_ref.is_some() || bs_spec.admin_credentials_ref.is_some();
    if nothing_to_drop || !has_admin {
        remove_finalizer(ctx, cr.as_ref(), svc_name, ns).await?;
        return Ok(Action::await_change());
    }

    let owner = owner_reference(cr.as_ref())?;
    // ServiceAccount may have been GCed already; SSA recreates it idempotently.
    let sa = bootstrap::render_service_account(svc_name, ns, owner.clone());
    apply_service_account(ctx, ns, &sa).await?;

    // resolve the admin DSN the same way the bootstrap path does: either
    // BYO single-DSN secretKeyRef or a literal composed from a component-
    // style Secret. Required so a teardown after the user migrates from one
    // admin form to the other still has a valid DSN to authenticate with.
    let secret_api: Api<Secret> = Api::namespaced(ctx.client.clone(), ns);
    let resolved_admin = resolve_admin_dsn(&secret_api, bs_spec, &cr.spec.config).await?;

    let job_name = crate::children::labels::teardown_job_name(svc_name);
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), ns);
    let existing = job_api.get_opt(&job_name).await?;
    let job = bootstrap::render_teardown_job(cr.as_ref(), &ctx.runtime_image, &resolved_admin.dsn, policy, owner)?;
    let Some(existing) = existing else {
        job_api
            .patch(
                &job_name,
                &PatchParams::apply(&ctx.field_manager).force(),
                &Patch::Apply(&job),
            )
            .await?;
        return Ok(Action::requeue(Duration::from_secs(10)));
    };
    let succeeded = existing.status.as_ref().and_then(|s| s.succeeded).unwrap_or(0);
    if succeeded >= 1 {
        remove_finalizer(ctx, cr.as_ref(), svc_name, ns).await?;
        return Ok(Action::await_change());
    }
    Ok(Action::requeue(Duration::from_secs(10)))
}

async fn ensure_finalizer(ctx: &Ctx, cr: &MarsService, svc_name: &str, ns: &str) -> Result<()> {
    let already = cr
        .metadata
        .finalizers
        .as_ref()
        .map(|f| f.iter().any(|s| s == BOOTSTRAP_FINALIZER))
        .unwrap_or(false);
    if already {
        return Ok(());
    }
    let api: Api<MarsService> = Api::namespaced(ctx.client.clone(), ns);
    let patch = json!({
        "metadata": {
            "finalizers": [BOOTSTRAP_FINALIZER]
        }
    });
    api.patch(svc_name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

async fn remove_finalizer(ctx: &Ctx, cr: &MarsService, svc_name: &str, ns: &str) -> Result<()> {
    let mut remaining: Vec<String> = cr
        .metadata
        .finalizers
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|f| f != BOOTSTRAP_FINALIZER)
        .collect();
    // serialize_json::Value doesn't differentiate empty Vec from None for the
    // PATCH. Use null when empty so the field is cleared.
    let api: Api<MarsService> = Api::namespaced(ctx.client.clone(), ns);
    let value = if remaining.is_empty() {
        json!({ "metadata": { "finalizers": serde_json::Value::Null } })
    } else {
        remaining.sort();
        json!({ "metadata": { "finalizers": remaining } })
    };
    api.patch(svc_name, &PatchParams::default(), &Patch::Merge(&value))
        .await?;
    Ok(())
}

async fn apply_service_account(ctx: &Ctx, ns: &str, sa: &ServiceAccount) -> Result<()> {
    let api: Api<ServiceAccount> = Api::namespaced(ctx.client.clone(), ns);
    let name = sa.metadata.name.as_deref().unwrap_or("");
    api.patch(name, &PatchParams::apply(&ctx.field_manager).force(), &Patch::Apply(sa))
        .await?;
    Ok(())
}

fn owner_reference(cr: &MarsService) -> Result<OwnerReference> {
    let uid = cr
        .metadata
        .uid
        .clone()
        .ok_or_else(|| OperatorError::MissingField("metadata.uid".into()))?;
    let name = cr
        .metadata
        .name
        .clone()
        .ok_or_else(|| OperatorError::MissingField("metadata.name".into()))?;
    Ok(OwnerReference {
        api_version: MarsService::api_version(&()).into_owned(),
        kind: MarsService::kind(&()).into_owned(),
        name,
        uid,
        controller: Some(true),
        block_owner_deletion: Some(true),
    })
}

fn detect_fs_store(cr: &MarsService) -> Option<ArtifactStoreSpec> {
    let store_type = cr
        .spec
        .config
        .get("artifacts")
        .and_then(|a| a.get("store"))
        .and_then(|s| s.get("type"))
        .and_then(|v| v.as_str());
    if store_type == Some("fs") {
        Some(cr.spec.artifact_store.clone().unwrap_or_default())
    } else {
        None
    }
}

/// fs store + multi-replica runtime requires RWX access. Returning Some()
/// means the operator refuses to roll children and surfaces a Degraded
/// condition instead - that is a hard constraint, not a warning.
fn artifact_store_guard(cr: &MarsService, fs_store: Option<&ArtifactStoreSpec>) -> Option<String> {
    let art = fs_store?;
    if cr.spec.runtime.replicas > 1 && !art.access_modes.iter().any(|m| m == "ReadWriteMany") {
        Some(format!(
            "artifacts.store.type=fs with runtime.replicas={} requires accessModes to include ReadWriteMany (got {:?})",
            cr.spec.runtime.replicas, art.access_modes
        ))
    } else {
        None
    }
}

async fn apply_configmap(ctx: &Ctx, ns: &str, cm: &ConfigMap) -> Result<()> {
    let api: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), ns);
    let name = cm.metadata.name.as_deref().unwrap_or("");
    api.patch(name, &PatchParams::apply(&ctx.field_manager).force(), &Patch::Apply(cm))
        .await?;
    Ok(())
}

async fn apply_pvc(ctx: &Ctx, ns: &str, pvc: &PersistentVolumeClaim) -> Result<()> {
    let api: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), ns);
    let name = pvc.metadata.name.as_deref().unwrap_or("");
    // create-only: PVC spec fields are largely immutable. if it exists we
    // leave it alone; mismatch surfaces via observed object events.
    if api.get_opt(name).await?.is_some() {
        return Ok(());
    }
    api.patch(
        name,
        &PatchParams::apply(&ctx.field_manager).force(),
        &Patch::Apply(pvc),
    )
    .await?;
    Ok(())
}

async fn apply_deployment(ctx: &Ctx, ns: &str, dep: &Deployment) -> Result<()> {
    let api: Api<Deployment> = Api::namespaced(ctx.client.clone(), ns);
    let name = dep.metadata.name.as_deref().unwrap_or("");
    api.patch(
        name,
        &PatchParams::apply(&ctx.field_manager).force(),
        &Patch::Apply(dep),
    )
    .await?;
    Ok(())
}

async fn apply_service(ctx: &Ctx, ns: &str, svc: &Service) -> Result<()> {
    let api: Api<Service> = Api::namespaced(ctx.client.clone(), ns);
    let name = svc.metadata.name.as_deref().unwrap_or("");
    api.patch(
        name,
        &PatchParams::apply(&ctx.field_manager).force(),
        &Patch::Apply(svc),
    )
    .await?;
    Ok(())
}

async fn patch_status(ctx: &Ctx, name: &str, ns: &str, status_body: crate::crd::MarsServiceStatus) -> Result<()> {
    let api: Api<MarsService> = Api::namespaced(ctx.client.clone(), ns);
    let body = json!({ "status": status_body });
    api.patch_status(
        name,
        &PatchParams::apply(&ctx.field_manager).force(),
        &Patch::Merge(&body),
    )
    .await?;
    Ok(())
}

pub(crate) fn error_policy(_cr: Arc<MarsService>, error: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    error!(error = %error, "reconcile error_policy fired");
    Action::requeue(Duration::from_secs(15))
}
