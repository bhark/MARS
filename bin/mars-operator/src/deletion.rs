//! Teardown flow and finalizer mutation. Runs when `metadata.deletionTimestamp`
//! is set; renders the teardown Job (idempotent via SSA), waits for completion,
//! then removes the finalizer so cascade-delete can proceed.

use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{Api, DeleteParams, Patch, PatchParams};
use kube::runtime::controller::Action;
use serde_json::json;

use crate::apply;
use crate::bootstrap::{self, BOOTSTRAP_FINALIZER};
use crate::bootstrap_flow;
use crate::children::labels;
use crate::crd::spec::MarsService;
use crate::error::{OperatorError, Result};
use crate::reconcile::{Ctx, owner_reference};

pub(crate) async fn reconcile_deletion(
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

    // shut down compiler + runtime before running the teardown Job. the
    // compiler holds the logical replication slot open; pg_drop_replication_slot
    // fails with "slot is active for PID N" until the consumer disconnects.
    // we drive the deletion, wait for the Deployments to be gone, then proceed.
    if shutdown_workloads(ctx, ns, svc_name).await? {
        return Ok(Action::requeue(Duration::from_secs(2)));
    }

    let owner = owner_reference(cr.as_ref())?;
    // ServiceAccount may have been GCed already; SSA recreates it idempotently.
    let sa = bootstrap::render_service_account(svc_name, ns, owner.clone());
    apply::service_account(ctx, ns, &sa).await?;

    // resolve the admin DSN the same way the bootstrap path does. Both BYO
    // adminSecretRef and component-style adminCredentialsRef end up as a
    // SecretKeyRef; the latter re-materialises the managed
    // <svc>-bootstrap-admin-credentials Secret so a teardown after the user
    // migrates between admin forms still authenticates with a valid DSN.
    let secret_api: Api<Secret> = Api::namespaced(ctx.client.clone(), ns);
    let resolved_admin =
        bootstrap_flow::resolve_admin_dsn(ctx, &secret_api, svc_name, ns, bs_spec, &cr.spec.config, owner.clone())
            .await?;

    let job_name = labels::teardown_job_name(svc_name);
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), ns);
    let existing = job_api.get_opt(&job_name).await?;
    let job = bootstrap::render_teardown_job(
        cr.as_ref(),
        &ctx.runtime_image,
        &resolved_admin.admin_dsn_ref,
        policy,
        owner,
    )?;
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

/// Delete the compiler + runtime Deployments. Returns `true` if either was
/// still present (so the caller should requeue and re-check), `false` once
/// both are confirmed gone. Idempotent: re-deleting a missing Deployment is a
/// no-op. We can't rely on owner-reference GC here because the MarsService
/// keeps a finalizer through teardown and `blockOwnerDeletion` would otherwise
/// trap the children behind the parent.
async fn shutdown_workloads(ctx: &Ctx, ns: &str, svc_name: &str) -> Result<bool> {
    let api: Api<Deployment> = Api::namespaced(ctx.client.clone(), ns);
    let mut still_present = false;
    for name in [
        labels::compiler_deployment_name(svc_name),
        labels::runtime_deployment_name(svc_name),
    ] {
        match api.get_opt(&name).await? {
            None => continue,
            Some(_) => {
                still_present = true;
                match api.delete(&name, &DeleteParams::default()).await {
                    Ok(_) => {}
                    Err(kube::Error::Api(e)) if e.code == 404 => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
    Ok(still_present)
}

pub(crate) async fn ensure_finalizer(ctx: &Ctx, cr: &MarsService, svc_name: &str, ns: &str) -> Result<()> {
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
