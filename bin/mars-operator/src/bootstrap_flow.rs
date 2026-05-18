//! Bootstrap orchestration: drive the state machine that produces a
//! `BootstrapOutcome`, and own the Secrets it needs (operator-managed admin
//! DSN composition + auto-generated runtime password). `bootstrap.rs` is the
//! pure rendering layer (Job/SA builders, plan hash); this file is the
//! reconcile-time orchestrator that calls into it.

use std::time::Duration;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{Api, Patch, PatchParams};
use tracing::info;

use crate::bootstrap::{self, PlanInputs};
use crate::children::labels::{self, BOOTSTRAP_ADMIN_DSN_KEY};
use crate::crd::{BootstrapSpec, MarsService, SecretKeyRef};
use crate::error::{OperatorError, Result};
use crate::reconcile::Ctx;
use crate::status::{BootstrapReason, BootstrapStatus};
use crate::{apply, deletion, dsn};

/// Outcome of the bootstrap reconciliation step. `proceed = false` halts the
/// reconcile here and surfaces the condition to the user without applying
/// compiler/runtime children.
pub(crate) struct BootstrapOutcome {
    pub(crate) proceed: bool,
    pub(crate) status: BootstrapStatus<'static>,
    pub(crate) requeue: Duration,
    /// Resolved Secret holding the runtime role password. Always Some when
    /// `spec.bootstrap` is declared (BYO or operator-managed); None means
    /// the legacy "no bootstrap" path and no MARS_RUNTIME_PASSWORD env is
    /// projected into compiler/runtime pods.
    pub(crate) runtime_password_ref: Option<SecretKeyRef>,
    /// Name of the operator-managed admin-credentials Secret holding the
    /// composed DSN. Some only on the component-style `adminCredentialsRef`
    /// branch; surfaced on `status.bootstrapAdminCredentialsSecret`.
    pub(crate) bootstrap_admin_credentials_secret: Option<String>,
}

pub(crate) async fn reconcile_bootstrap(
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
                bootstrap_admin_credentials_secret: None,
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
                bootstrap_admin_credentials_secret: None,
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
            bootstrap_admin_credentials_secret: None,
        });
    }

    // resolve admin + runtime secret resourceVersions so the plan hash rolls
    // when either secret is rotated.
    let secret_api: Api<Secret> = Api::namespaced(ctx.client.clone(), ns);
    let resolved_admin =
        resolve_admin_dsn(ctx, &secret_api, svc_name, ns, bs_spec, &cr.spec.config, owner.clone()).await?;
    let runtime_password_ref = ensure_runtime_password_secret(ctx, svc_name, ns, bs_spec, owner.clone()).await?;
    let runtime_rv = secret_api
        .get_opt(&runtime_password_ref.name)
        .await?
        .and_then(|s| s.metadata.resource_version.clone())
        .unwrap_or_default();

    let managed_admin_secret = resolved_admin.managed_secret_name.clone();
    let inputs = PlanInputs {
        source_bootstrap: source_bs,
        runtime_password_ref: runtime_password_ref.clone(),
        admin_dsn_ref: resolved_admin.admin_dsn_ref.clone(),
        admin_secret_resource_version: resolved_admin.resolved_secret_resource_version,
        runtime_secret_resource_version: runtime_rv,
    };
    let hash = bootstrap::plan_hash(&inputs);

    // ServiceAccount for the Job. SSA so re-applies are no-ops.
    let sa = bootstrap::render_service_account(svc_name, ns, owner.clone());
    apply::service_account(ctx, ns, &sa).await?;

    // ensure or observe the Job for this hash.
    let job_name = labels::bootstrap_job_name(svc_name, &hash);
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
            bootstrap_admin_credentials_secret: managed_admin_secret,
        });
    };
    let st = existing.status.as_ref();
    let succeeded = st.and_then(|s| s.succeeded).unwrap_or(0);
    let failed = st.and_then(|s| s.failed).unwrap_or(0);

    if succeeded >= 1 {
        // mark the finalizer so a future delete runs teardown.
        deletion::ensure_finalizer(ctx, cr, svc_name, ns).await?;
        Ok(BootstrapOutcome {
            proceed: true,
            status: BootstrapStatus {
                ready: true,
                reason: BootstrapReason::Ready,
                message: "bootstrap Job succeeded",
            },
            requeue: Duration::from_secs(60),
            runtime_password_ref: Some(runtime_password_ref),
            bootstrap_admin_credentials_secret: managed_admin_secret,
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
            bootstrap_admin_credentials_secret: managed_admin_secret,
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
            bootstrap_admin_credentials_secret: managed_admin_secret,
        })
    }
}

/// Outcome of admin-DSN resolution: a `SecretKeyRef` the Job consumes via
/// `secretKeyRef`, the resourceVersion of that Secret (drives plan_hash
/// rotation), and - for the component-style branch - the name of the
/// operator-managed Secret holding the composed DSN.
pub(crate) struct ResolvedAdminDsn {
    pub(crate) admin_dsn_ref: SecretKeyRef,
    pub(crate) resolved_secret_resource_version: String,
    /// Set only when the operator composed and persisted the DSN itself
    /// (component-style `adminCredentialsRef` branch). Surfaced on
    /// `status.bootstrapAdminCredentialsSecret`.
    pub(crate) managed_secret_name: Option<String>,
}

/// Validate `bootstrap.adminSecretRef` vs `bootstrap.adminCredentialsRef` (exactly
/// one is required when enabled), then resolve the result into a `SecretKeyRef`
/// the bootstrap/teardown Job can mount. The component-style branch reads the
/// user's Secret, composes the DSN by combining its keys with host/port/database
/// fallbacks parsed out of the bootstrap-bearing `spec.config.sources[].dsn`,
/// then persists the composed string into a managed Secret owned by the CR so
/// the DSN never appears outside a Secret resource.
pub(crate) async fn resolve_admin_dsn(
    ctx: &Ctx,
    secret_api: &Api<Secret>,
    svc_name: &str,
    ns: &str,
    bs_spec: &BootstrapSpec,
    config: &serde_json::Value,
    owner: OwnerReference,
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
                admin_dsn_ref: r.clone(),
                resolved_secret_resource_version: rv,
                managed_secret_name: None,
            })
        }
        (None, Some(creds)) => {
            let secret = secret_api.get_opt(&creds.secret_name).await?.ok_or_else(|| {
                OperatorError::ConfigInvalid(format!(
                    "bootstrap.adminCredentialsRef.secretName='{}' not found in namespace",
                    creds.secret_name
                ))
            })?;
            let data: std::collections::BTreeMap<String, Vec<u8>> = secret
                .data
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| (k, v.0))
                .collect();
            let fallback_dsn_src = bootstrap::source_dsn_for_fallback(config);
            let fallback = dsn::parse_dsn_components(&fallback_dsn_src);
            let composed = dsn::compose_admin_dsn(creds, &data, &fallback)
                .map_err(|e| OperatorError::ConfigInvalid(format!("compose admin DSN: {e}")))?;
            let (admin_dsn_ref, rv) =
                ensure_bootstrap_admin_credentials_secret(ctx, svc_name, ns, &composed, owner).await?;
            let managed_name = admin_dsn_ref.name.clone();
            Ok(ResolvedAdminDsn {
                admin_dsn_ref,
                resolved_secret_resource_version: rv,
                managed_secret_name: Some(managed_name),
            })
        }
    }
}

/// SSA-apply the operator-managed `<svc>-bootstrap-admin-credentials` Secret
/// holding the composed admin DSN. Returns the `SecretKeyRef` plus the
/// resourceVersion of the resulting Secret (server-side apply is a no-op when
/// bytes are unchanged, so the RV is stable across reconciles until the
/// composition itself changes).
async fn ensure_bootstrap_admin_credentials_secret(
    ctx: &Ctx,
    svc_name: &str,
    ns: &str,
    composed_dsn: &str,
    owner: OwnerReference,
) -> Result<(SecretKeyRef, String)> {
    let name = labels::bootstrap_admin_credentials_secret_name(svc_name);
    let key = BOOTSTRAP_ADMIN_DSN_KEY.to_string();
    let api: Api<Secret> = Api::namespaced(ctx.client.clone(), ns);
    let secret = build_bootstrap_admin_credentials_secret(&name, ns, svc_name, composed_dsn, owner);
    let patched = api
        .patch(
            &name,
            &PatchParams::apply(&ctx.field_manager).force(),
            &Patch::Apply(&secret),
        )
        .await?;
    let rv = patched.metadata.resource_version.unwrap_or_default();
    Ok((SecretKeyRef { name, key }, rv))
}

fn build_bootstrap_admin_credentials_secret(
    name: &str,
    ns: &str,
    svc: &str,
    composed_dsn: &str,
    owner: OwnerReference,
) -> Secret {
    use k8s_openapi::ByteString;
    let mut data = std::collections::BTreeMap::new();
    data.insert(
        BOOTSTRAP_ADMIN_DSN_KEY.into(),
        ByteString(composed_dsn.as_bytes().to_vec()),
    );
    Secret {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(name.into()),
            namespace: Some(ns.into()),
            labels: Some(labels::labels(svc, labels::COMPONENT_BOOTSTRAP_ADMIN_CREDENTIALS)),
            owner_references: Some(vec![owner]),
            ..Default::default()
        },
        data: Some(data),
        type_: Some("Opaque".into()),
        ..Default::default()
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
