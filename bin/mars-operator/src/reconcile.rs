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
use crate::bootstrap_flow;
use crate::children::labels::{self, artifact_store_pvc_name};
use crate::children::pvc::{self, PvcSpec};
use crate::children::{compiler, configmap, pdb, runtime, service};
use crate::compose::ComposeError;
use crate::crd::spec::{MarsService, SpecValidationError, validate_spec};
use crate::crd::storage::ArtifactStoreSpec;
use crate::deletion;
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
        OperatorError::SpecInvalid(_) => "spec_invalid",
        OperatorError::ClusterNotFound(_) => "cluster_not_found",
        OperatorError::DefinitionResolve(_) => "definition_resolve",
        OperatorError::DefinitionFetch(_) => "definition_fetch",
        OperatorError::DefinitionDecode(_) => "definition_decode",
        OperatorError::Compose(_) => "compose",
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

    // deletion handling runs before anything else: we must run a teardown
    // Job (if needed) and remove the finalizer before kube cascade-deletes
    // the children. nothing else can succeed once deletionTimestamp is set.
    if cr.metadata.deletion_timestamp.is_some() {
        if let Some(u) = uid.as_deref() {
            ctx.poller.unregister(u);
        }
        return deletion::reconcile_deletion(cr.clone(), &ctx, &svc_name, &ns).await;
    }

    // admission: legacy XOR new-shape. typed errors map onto the catalog +
    // definition resolution conditions so the user sees which prereq failed.
    if let Err(e) = validate_spec(&cr.spec) {
        let msg = e.to_string();
        warn!(svc = %svc_name, "spec admission failed: {msg}");
        let (catalog, definition) = classify_spec_validation(&e, &msg);
        return surface_resolution_failure(&ctx, &svc_name, &ns, generation, catalog, definition, &msg).await;
    }

    // resolve the effective config: legacy returns spec.config verbatim, the
    // new path composes cluster + render-definition into a Config.
    let is_legacy = cr.spec.config.is_some();
    if is_legacy {
        warn!(
            deprecated_field = "spec.config",
            target_version = "v1alpha2",
            service = %svc_name,
            namespace = %ns,
            "MarsService uses deprecated spec.config; migrate to clusterRef + definition + sources before v1alpha2"
        );
    }
    let (effective_cfg, resolved_def) = match resolve_effective_config(&cr, &ctx, &ns, uid.as_deref()).await {
        Ok(out) => out,
        Err(e) => {
            let msg = e.to_string();
            warn!(svc = %svc_name, "effective config: {msg}");
            let (catalog, definition) = classify_resolve_error(&e, is_legacy, &msg);
            return surface_resolution_failure(&ctx, &svc_name, &ns, generation, catalog, definition, &msg).await;
        }
    };

    let config_message = match resolved_def.as_ref() {
        Some(rd) => format!(
            "composed from cluster + {} definition (revision {})",
            rd.adapter, rd.revision
        ),
        None => "spec.config validated".to_string(),
    };

    if let Err(e) = crate::config::validate(&effective_cfg) {
        let msg = e.to_string();
        warn!(svc = %svc_name, "effective config invalid: {msg}");
        // catalog + definition both resolved fine; the composed config is what
        // failed downstream. emit them as True so the user sees the blocker
        // sits in ConfigValid, not in the upstream prereqs.
        let (catalog, definition) = if is_legacy {
            (Resolution::Legacy, Resolution::Legacy)
        } else {
            (Resolution::Resolved, Resolution::Resolved)
        };
        return surface_config_invalid(
            &ctx,
            &svc_name,
            &ns,
            generation,
            catalog,
            definition,
            resolved_def.as_ref(),
            &msg,
        )
        .await;
    }

    let (catalog_res, def_res) = resolution_pair(is_legacy);
    let observed = resolved_def.as_ref().map(observed_from);

    let fs_store = detect_fs_store(&effective_cfg, &cr);

    if let Some(reason) = artifact_store_guard(&cr, fs_store.as_ref()) {
        warn!(svc = %svc_name, "degraded: {reason}");
        let status_body = status::compute(StatusInputs {
            observed_generation: generation,
            catalog: catalog_res,
            definition: def_res,
            definition_observed: observed,
            config_valid: true,
            config_message: &config_message,
            children_applied: false,
            children_message: "skipped: degraded",
            compiler_ready: false,
            runtime_ready: false,
            degraded: Some(&reason),
            bootstrap: None,
            runtime_credentials_secret: None,
            bootstrap_admin_credentials_secret: None,
        });
        apply::patch_status(&ctx, &svc_name, &ns, status_body).await?;
        return Ok(Action::requeue(Duration::from_secs(30)));
    }

    // configmap must exist before the bootstrap Job pod can mount it. PVCs
    // come later because the bootstrap Job does not need them.
    let (cm, checksum) = configmap::build(&cr, &effective_cfg, owner_ref.clone())?;
    apply::configmap(&ctx, &ns, &cm).await?;

    let bootstrap_outcome =
        bootstrap_flow::reconcile_bootstrap(&ctx, &cr, &svc_name, &ns, &effective_cfg, owner_ref.clone()).await?;
    let runtime_credentials_secret = bootstrap_outcome.runtime_password_ref.as_ref().map(|r| r.name.as_str());
    let bootstrap_admin_credentials_secret = bootstrap_outcome.bootstrap_admin_credentials_secret.as_deref();
    if !bootstrap_outcome.proceed {
        let status_body = status::compute(StatusInputs {
            observed_generation: generation,
            catalog: catalog_res,
            definition: def_res,
            definition_observed: observed,
            config_valid: true,
            config_message: &config_message,
            children_applied: false,
            children_message: "skipped: bootstrap not ready",
            compiler_ready: false,
            runtime_ready: false,
            degraded: None,
            bootstrap: Some(bootstrap_outcome.status),
            runtime_credentials_secret,
            bootstrap_admin_credentials_secret,
        });
        apply::patch_status(&ctx, &svc_name, &ns, status_body).await?;
        return Ok(Action::requeue(bootstrap_outcome.requeue));
    }

    if let Some(art) = &fs_store {
        let art_pvc = pvc::build(
            PvcSpec {
                name: &artifact_store_pvc_name(&svc_name),
                namespace: Some(&ns),
                labels: labels::labels(&svc_name, "artifact-store"),
                size: &art.size,
                storage_class: &art.storage_class,
                access_modes: &art.access_modes,
            },
            owner_ref.clone(),
        );
        apply::pvc(&ctx, &ns, &art_pvc).await?;
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
    apply::pvc(&ctx, &ns, &compiler_children.cache_pvc).await?;
    apply::pvc(&ctx, &ns, &compiler_children.work_pvc).await?;
    apply::deployment(&ctx, &ns, &compiler_children.deployment).await?;

    let runtime_deployment = runtime::build(
        &cr,
        &checksum,
        fs_store.as_ref(),
        runtime_password_ref,
        &ctx.runtime_image,
        owner_ref.clone(),
    )?;
    apply::deployment(&ctx, &ns, &runtime_deployment).await?;

    let runtime_service = service::build(&cr, owner_ref.clone())?;
    apply::service(&ctx, &ns, &runtime_service).await?;

    // sibling PDB: present when spec.runtime.podDisruptionBudget is set,
    // removed otherwise so clearing the field garbage-collects the prior one.
    let runtime_pdb = pdb::build(&cr, owner_ref)?;
    apply::runtime_pdb(&ctx, &ns, &svc_name, runtime_pdb.as_ref()).await?;

    // re-read deployments for readiness
    let dep_api: Api<Deployment> = Api::namespaced(ctx.client.clone(), &ns);
    let compiler_dep = dep_api.get_opt(&labels::compiler_deployment_name(&svc_name)).await?;
    let runtime_dep = dep_api.get_opt(&labels::runtime_deployment_name(&svc_name)).await?;

    let compiler_ready = compiler_dep.as_ref().map(status::deployment_ready).unwrap_or(false);
    let runtime_ready = runtime_dep.as_ref().map(status::deployment_ready).unwrap_or(false);

    let status_body = status::compute(StatusInputs {
        observed_generation: generation,
        catalog: catalog_res,
        definition: def_res,
        definition_observed: observed,
        config_valid: true,
        config_message: &config_message,
        children_applied: true,
        children_message: "all children applied",
        compiler_ready,
        runtime_ready,
        degraded: None,
        bootstrap: Some(bootstrap_outcome.status),
        runtime_credentials_secret,
        bootstrap_admin_credentials_secret,
    });
    apply::patch_status(&ctx, &svc_name, &ns, status_body).await?;

    Ok(Action::requeue(Duration::from_secs(30)))
}

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

fn detect_fs_store(effective_cfg: &JsonValue, cr: &MarsService) -> Option<ArtifactStoreSpec> {
    let store_type = effective_cfg
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

pub(crate) fn error_policy(_cr: Arc<MarsService>, error: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    error!(error = %error, "reconcile error_policy fired");
    Action::requeue(Duration::from_secs(15))
}

/// Upstream resolution failed (admission, catalog lookup, definition
/// fetch/parse). Emits the matching `CatalogResolved` / `DefinitionResolved`
/// state plus a `ConfigValid=False` blocker so downstream conditions stay
/// skipped. `definition_observed` is intentionally cleared - resolution
/// failed, so the last fetched identity (if any) is not load-bearing here.
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
        bootstrap: None,
        runtime_credentials_secret: None,
        bootstrap_admin_credentials_secret: None,
    });
    apply::patch_status(ctx, svc_name, ns, status_body).await?;
    Ok(Action::requeue(Duration::from_secs(30)))
}

/// Composed-config validation failed; catalog + definition resolved fine.
/// Keeps the prior observed identity so consumers see what bytes the
/// operator parsed before the composed Config tripped validation.
#[allow(clippy::too_many_arguments)]
async fn surface_config_invalid(
    ctx: &Ctx,
    svc_name: &str,
    ns: &str,
    generation: i64,
    catalog: Resolution<'_>,
    definition: Resolution<'_>,
    resolved_def: Option<&ResolvedDefinition>,
    message: &str,
) -> Result<Action> {
    let observed = resolved_def.map(observed_from);
    let status_body = status::compute(StatusInputs {
        observed_generation: generation,
        catalog,
        definition,
        definition_observed: observed,
        config_valid: false,
        config_message: message,
        children_applied: false,
        children_message: "skipped: config invalid",
        compiler_ready: false,
        runtime_ready: false,
        degraded: None,
        bootstrap: None,
        runtime_credentials_secret: None,
        bootstrap_admin_credentials_secret: None,
    });
    apply::patch_status(ctx, svc_name, ns, status_body).await?;
    Ok(Action::requeue(Duration::from_secs(30)))
}

/// Both resolution conditions report True (legacy variant on the legacy path,
/// resolved on the new path).
fn resolution_pair<'a>(is_legacy: bool) -> (Resolution<'a>, Resolution<'a>) {
    if is_legacy {
        (Resolution::Legacy, Resolution::Legacy)
    } else {
        (Resolution::Resolved, Resolution::Resolved)
    }
}

fn observed_from(rd: &ResolvedDefinition) -> ObservedDefinition<'_> {
    ObservedDefinition {
        adapter: rd.adapter,
        revision: rd.revision.as_str(),
    }
}

/// Map admission failures onto the two resolution conditions. `BothShapes` /
/// `NeitherShape` are ambiguous prereqs - they block both. The new-shape
/// "missing field" errors map to whichever condition the missing field belongs
/// to. The exactly-one definition-variant violation maps to
/// DefinitionResolved=False with the dedicated reason.
fn classify_spec_validation<'a>(e: &SpecValidationError, msg: &'a str) -> (Resolution<'a>, Resolution<'a>) {
    use SpecValidationError as E;
    match e {
        E::BothShapes | E::NeitherShape | E::BootstrapOnNewPath => (
            Resolution::Failed {
                reason: ResolutionReason::SpecInvalid,
                message: msg,
            },
            Resolution::Failed {
                reason: ResolutionReason::SpecInvalid,
                message: msg,
            },
        ),
        E::NewShapeMissing("definition") => (
            Resolution::Resolved,
            Resolution::Failed {
                reason: ResolutionReason::SpecInvalid,
                message: msg,
            },
        ),
        E::NewShapeMissing(_) => (
            Resolution::Failed {
                reason: ResolutionReason::SpecInvalid,
                message: msg,
            },
            Resolution::Skipped {
                blocked_by: "CatalogResolved",
            },
        ),
        E::DefinitionVariantCount(_) => (
            Resolution::Resolved,
            Resolution::Failed {
                reason: ResolutionReason::ExactlyOneViolated,
                message: msg,
            },
        ),
    }
}

/// Map a runtime resolution `OperatorError` onto the two conditions. The
/// classification mirrors the failure point inside `resolve_effective_config`:
/// cluster lookup feeds CatalogResolved, definition resolve/fetch/decode feed
/// DefinitionResolved, and `ComposeError::UnknownSourceId` lands on
/// CatalogResolved because the catalog cannot satisfy the spec.
fn classify_resolve_error<'a>(e: &OperatorError, is_legacy: bool, msg: &'a str) -> (Resolution<'a>, Resolution<'a>) {
    if is_legacy {
        // legacy path: resolution doesn't apply. surface failure on definition
        // to keep one consistent slot ("legacy + something broke").
        return (
            Resolution::Legacy,
            Resolution::Failed {
                reason: ResolutionReason::Internal,
                message: msg,
            },
        );
    }
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

/// Resolve the effective Config for `cr`. The legacy path returns
/// `spec.config` verbatim and skips poller registration. The new path fetches
/// the cluster CR, registers a poller for credentialed definition sources, and
/// composes the resolved RenderDefinition with the cluster catalog into a
/// `Config`. Returns the value plus, on the new path, the resolved-definition
/// identity for status reporting.
async fn resolve_effective_config(
    cr: &MarsService,
    ctx: &Ctx,
    ns: &str,
    uid: Option<&str>,
) -> Result<(JsonValue, Option<effective_config::ResolvedDefinition>)> {
    if cr.spec.config.is_some() {
        // legacy path: no poller, no cluster lookup.
        if let Some(u) = uid {
            ctx.poller.unregister(u);
        }
        return Ok((effective_config::legacy(cr)?, None));
    }
    // new path. validate_spec has already enforced clusterRef + definition presence.
    if let (Some(u), Some(def_spec)) = (uid, cr.spec.definition.as_ref()) {
        ctx.poller.register(u, ns, &cr_name(cr)?, def_spec, &ctx.client).await?;
    }
    let out = effective_config::new(cr, &ctx.client, ns).await?;
    Ok((out.config, Some(out.definition)))
}

fn cr_name(cr: &MarsService) -> Result<String> {
    cr.metadata
        .name
        .clone()
        .ok_or_else(|| OperatorError::MissingField("metadata.name".into()))
}

#[cfg(test)]
mod tests;
