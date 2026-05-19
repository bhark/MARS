//! Reconcile loop: validate spec, render children, server-side apply,
//! refresh status. Pure of side-effects beyond the kube client and the
//! metrics facade.

use std::sync::Arc;
use std::time::{Duration, Instant};

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::Resource;
use kube::api::Api;
use kube::runtime::controller::Action;
use serde_json::Value as JsonValue;
use tracing::{error, info, warn};

use crate::apply;
use crate::children::labels::{self, artifact_store_pvc_name};
use crate::children::pvc::{self, PvcSpec};
use crate::children::{compiler, configmap, pdb, runtime, service};
use crate::compose::ComposeError;
use crate::crd::spec::{MarsService, SpecValidationError, validate_spec};
use crate::effective_config::{self, ResolvedDefinition};
use crate::error::{OperatorError, Result};
use crate::metrics::Metrics;
use crate::poller::PollerManager;
use crate::status::{self, ObservedDefinition, Resolution, ResolutionReason, StatusInputs};

pub(crate) struct Ctx {
    pub(crate) client: kube::Client,
    pub(crate) field_manager: String,
    pub(crate) metrics: Metrics,
    /// `repo:version` for the runtime/compiler containers. Built once at
    /// startup from CLI/env + operator's own CARGO_PKG_VERSION; identical
    /// for every reconcile.
    pub(crate) runtime_image: String,
    /// Per-CR poller table for `gitRef` / `s3Ref` definition sources.
    /// Registered on every reconcile pass; unregistered on deletion.
    pub(crate) poller: Arc<PollerManager>,
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
            ctx.metrics.record_error(e.kind());
            Err(e)
        }
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
    let uid = cr.metadata.uid.clone();

    info!(svc = %svc_name, ns = %ns, gen = generation, "reconciling MarsService");

    let owner_ref = owner_reference(&cr)?;

    // on delete, the cluster bootstrap reconciler owns lifecycle of any
    // postgres-side state; per-service teardown is handled by cascade GC on
    // the owned children. unregister the poller so it does not leak.
    if cr.metadata.deletion_timestamp.is_some() {
        if let Some(u) = uid.as_deref() {
            ctx.poller.unregister(u);
        }
        return Ok(Action::await_change());
    }

    // admission: exactly-one definition variant. typed error maps onto the
    // DefinitionResolved condition.
    if let Err(e) = validate_spec(&cr.spec) {
        let msg = e.to_string();
        warn!(svc = %svc_name, "spec admission failed: {msg}");
        let (catalog, definition) = classify_spec_validation(&e, &msg);
        return surface_resolution_failure(&ctx, &svc_name, &ns, generation, catalog, definition, &msg).await;
    }

    // register the per-CR poller for credentialed definition sources. inline /
    // configMapRef variants are handled inline by the kube watch fan-in.
    if let Some(u) = uid.as_deref() {
        ctx.poller
            .register(u, &ns, &svc_name, &cr.spec.definition, &ctx.client)
            .await?;
    }

    let resolved = match effective_config::resolve(&cr, &ctx.client, &ns).await {
        Ok(out) => out,
        Err(e) => {
            let msg = e.to_string();
            warn!(svc = %svc_name, "effective config: {msg}");
            let (catalog, definition) = classify_resolve_error(&e, &msg);
            return surface_resolution_failure(&ctx, &svc_name, &ns, generation, catalog, definition, &msg).await;
        }
    };

    let config_message = format!(
        "composed from cluster + {} definition (revision {})",
        resolved.definition.adapter, resolved.definition.revision
    );

    if let Err(e) = crate::config::validate(&resolved.config) {
        let msg = e.to_string();
        warn!(svc = %svc_name, "effective config invalid: {msg}");
        return surface_config_invalid(&ctx, &svc_name, &ns, generation, &resolved.definition, &msg).await;
    }

    let observed = Some(observed_from(&resolved.definition));
    let fs_store = detect_fs_store(&resolved.config);

    if let Some(reason) = artifact_store_guard(&cr, fs_store) {
        warn!(svc = %svc_name, "degraded: {reason}");
        let status_body = status::compute(StatusInputs {
            observed_generation: generation,
            catalog: Resolution::Resolved,
            definition: Resolution::Resolved,
            definition_observed: observed,
            config_valid: true,
            config_message: &config_message,
            children_applied: false,
            children_message: "skipped: degraded",
            compiler_ready: false,
            runtime_ready: false,
            degraded: Some(&reason),
        });
        apply::patch_status(&ctx, &svc_name, &ns, status_body).await?;
        return Ok(Action::requeue(Duration::from_secs(30)));
    }

    let (cm, checksum) = configmap::build(&cr, &resolved.config, owner_ref.clone())?;
    apply::configmap(&ctx, &ns, &cm).await?;

    if fs_store {
        let access_modes = [ARTIFACT_STORE_PVC_ACCESS_MODE.to_string()];
        let art_pvc = pvc::build(
            PvcSpec {
                name: &artifact_store_pvc_name(&svc_name),
                namespace: Some(&ns),
                labels: labels::labels(&svc_name, "artifact-store"),
                size: ARTIFACT_STORE_PVC_SIZE,
                storage_class: "",
                access_modes: &access_modes,
            },
            owner_ref.clone(),
        );
        apply::pvc(&ctx, &ns, &art_pvc).await?;
    }

    let compiler_children = compiler::build(&cr, &checksum, fs_store, &ctx.runtime_image, owner_ref.clone())?;
    apply::pvc(&ctx, &ns, &compiler_children.cache_pvc).await?;
    apply::pvc(&ctx, &ns, &compiler_children.work_pvc).await?;
    apply::deployment(&ctx, &ns, &compiler_children.deployment).await?;

    let runtime_deployment = runtime::build(&cr, &checksum, fs_store, &ctx.runtime_image, owner_ref.clone())?;
    apply::deployment(&ctx, &ns, &runtime_deployment).await?;

    let runtime_service = service::build(&cr, owner_ref.clone())?;
    apply::service(&ctx, &ns, &runtime_service).await?;

    let runtime_pdb = pdb::build(&cr, owner_ref)?;
    apply::runtime_pdb(&ctx, &ns, &svc_name, runtime_pdb.as_ref()).await?;

    let dep_api: Api<Deployment> = Api::namespaced(ctx.client.clone(), &ns);
    let compiler_dep = dep_api.get_opt(&labels::compiler_deployment_name(&svc_name)).await?;
    let runtime_dep = dep_api.get_opt(&labels::runtime_deployment_name(&svc_name)).await?;

    let compiler_ready = compiler_dep.as_ref().map(status::deployment_ready).unwrap_or(false);
    let runtime_ready = runtime_dep.as_ref().map(status::deployment_ready).unwrap_or(false);

    let status_body = status::compute(StatusInputs {
        observed_generation: generation,
        catalog: Resolution::Resolved,
        definition: Resolution::Resolved,
        definition_observed: observed,
        config_valid: true,
        config_message: &config_message,
        children_applied: true,
        children_message: "all children applied",
        compiler_ready,
        runtime_ready,
        degraded: None,
    });
    apply::patch_status(&ctx, &svc_name, &ns, status_body).await?;

    Ok(Action::requeue(Duration::from_secs(30)))
}

/// PVC defaults for the fs-store artifact volume. Per-service overrides
/// belong on the cluster catalog when the need surfaces; until then the
/// operator picks one shape that works for single-replica deployments and
/// surfaces a Degraded condition for multi-replica + RWO.
const ARTIFACT_STORE_PVC_SIZE: &str = "5Gi";
const ARTIFACT_STORE_PVC_ACCESS_MODE: &str = "ReadWriteOnce";

pub(crate) fn owner_reference(cr: &MarsService) -> Result<OwnerReference> {
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

fn detect_fs_store(effective_cfg: &JsonValue) -> bool {
    effective_cfg
        .get("artifacts")
        .and_then(|a| a.get("store"))
        .and_then(|s| s.get("type"))
        .and_then(|v| v.as_str())
        == Some("fs")
}

/// fs store + multi-replica runtime requires RWX access. The operator owns
/// PVC sizing now (no service-side override), and the default access modes
/// are RWO; multi-replica with the default lands in a Degraded condition.
fn artifact_store_guard(cr: &MarsService, fs_store: bool) -> Option<String> {
    if fs_store && cr.spec.runtime.replicas > 1 {
        Some(format!(
            "artifacts.store.type=fs with runtime.replicas={} requires a ReadWriteMany volume; \
             current operator-managed PVC is ReadWriteOnce",
            cr.spec.runtime.replicas
        ))
    } else {
        None
    }
}

pub(crate) fn error_policy(_cr: Arc<MarsService>, error: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    error!(error = %error, "reconcile error_policy fired");
    Action::requeue(Duration::from_secs(15))
}

async fn surface_resolution_failure(
    ctx: &Ctx,
    svc_name: &str,
    ns: &str,
    generation: i64,
    catalog: Resolution<'_>,
    definition: Resolution<'_>,
    message: &str,
) -> Result<Action> {
    let status_body = status::compute(StatusInputs {
        observed_generation: generation,
        catalog,
        definition,
        definition_observed: None,
        config_valid: false,
        config_message: message,
        children_applied: false,
        children_message: "skipped: config invalid",
        compiler_ready: false,
        runtime_ready: false,
        degraded: None,
    });
    apply::patch_status(ctx, svc_name, ns, status_body).await?;
    Ok(Action::requeue(Duration::from_secs(30)))
}

async fn surface_config_invalid(
    ctx: &Ctx,
    svc_name: &str,
    ns: &str,
    generation: i64,
    resolved_def: &ResolvedDefinition,
    message: &str,
) -> Result<Action> {
    let status_body = status::compute(StatusInputs {
        observed_generation: generation,
        catalog: Resolution::Resolved,
        definition: Resolution::Resolved,
        definition_observed: Some(observed_from(resolved_def)),
        config_valid: false,
        config_message: message,
        children_applied: false,
        children_message: "skipped: config invalid",
        compiler_ready: false,
        runtime_ready: false,
        degraded: None,
    });
    apply::patch_status(ctx, svc_name, ns, status_body).await?;
    Ok(Action::requeue(Duration::from_secs(30)))
}

fn observed_from(rd: &ResolvedDefinition) -> ObservedDefinition<'_> {
    ObservedDefinition {
        adapter: rd.adapter,
        revision: rd.revision.as_str(),
    }
}

fn classify_spec_validation<'a>(e: &SpecValidationError, msg: &'a str) -> (Resolution<'a>, Resolution<'a>) {
    match e {
        SpecValidationError::DefinitionVariantCount(_) => (
            Resolution::Resolved,
            Resolution::Failed {
                reason: ResolutionReason::ExactlyOneViolated,
                message: msg,
            },
        ),
    }
}

fn classify_resolve_error<'a>(e: &OperatorError, msg: &'a str) -> (Resolution<'a>, Resolution<'a>) {
    match e {
        OperatorError::ClusterNotFound(_) => (
            Resolution::Failed {
                reason: ResolutionReason::ClusterNotFound,
                message: msg,
            },
            Resolution::Skipped {
                blocked_by: "CatalogResolved",
            },
        ),
        OperatorError::Compose(ComposeError::UnknownSourceId { .. }) => (
            Resolution::Failed {
                reason: ResolutionReason::UnknownSourceId,
                message: msg,
            },
            Resolution::Resolved,
        ),
        OperatorError::Compose(ComposeError::CatalogEntryMissingId { .. })
        | OperatorError::Compose(ComposeError::InvalidCatalogEntry { .. })
        | OperatorError::Compose(ComposeError::InvalidClusterField { .. }) => (
            Resolution::Failed {
                reason: ResolutionReason::InvalidCatalog,
                message: msg,
            },
            Resolution::Resolved,
        ),
        OperatorError::DefinitionResolve(_) => (
            Resolution::Resolved,
            Resolution::Failed {
                reason: ResolutionReason::DefinitionResolveError,
                message: msg,
            },
        ),
        OperatorError::DefinitionFetch(_) => (
            Resolution::Resolved,
            Resolution::Failed {
                reason: ResolutionReason::DefinitionFetchError,
                message: msg,
            },
        ),
        OperatorError::DefinitionDecode(_) => (
            Resolution::Resolved,
            Resolution::Failed {
                reason: ResolutionReason::DefinitionDecodeError,
                message: msg,
            },
        ),
        _ => (
            Resolution::Failed {
                reason: ResolutionReason::Internal,
                message: msg,
            },
            Resolution::Failed {
                reason: ResolutionReason::Internal,
                message: msg,
            },
        ),
    }
}

#[cfg(test)]
mod tests;
