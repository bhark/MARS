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
use tracing::{error, info, warn};

use crate::apply;
use crate::bootstrap_flow;
use crate::children::labels::{self, artifact_store_pvc_name};
use crate::children::pvc::{self, PvcSpec};
use crate::children::{compiler, configmap, pdb, runtime, service};
use crate::crd::spec::MarsService;
use crate::crd::storage::ArtifactStoreSpec;
use crate::deletion;
use crate::error::{OperatorError, Result};
use crate::metrics::Metrics;
use crate::status::{self, StatusInputs};

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
        return deletion::reconcile_deletion(cr.clone(), &ctx, &svc_name, &ns).await;
    }

    let (config_valid, config_message) = match cr.spec.config.as_ref() {
        Some(cfg) => match crate::config::validate(cfg) {
            Ok(()) => (true, "spec.config validated".to_string()),
            Err(e) => (false, e.to_string()),
        },
        // new-shape path lands in task 5; for now flag the absence so the
        // legacy reconcile doesn't barrel into a None.
        None => (false, "spec.config is required (new-shape path not yet wired)".into()),
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
            bootstrap_admin_credentials_secret: None,
        });
        apply::patch_status(&ctx, &svc_name, &ns, status_body).await?;
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
            bootstrap_admin_credentials_secret: None,
        });
        apply::patch_status(&ctx, &svc_name, &ns, status_body).await?;
        return Ok(Action::requeue(Duration::from_secs(30)));
    }

    // configmap must exist before the bootstrap Job pod can mount it. PVCs
    // come later because the bootstrap Job does not need them.
    let (cm, checksum) = configmap::build(&cr, owner_ref.clone())?;
    apply::configmap(&ctx, &ns, &cm).await?;

    let bootstrap_outcome = bootstrap_flow::reconcile_bootstrap(&ctx, &cr, &svc_name, &ns, owner_ref.clone()).await?;
    let runtime_credentials_secret = bootstrap_outcome.runtime_password_ref.as_ref().map(|r| r.name.as_str());
    let bootstrap_admin_credentials_secret = bootstrap_outcome.bootstrap_admin_credentials_secret.as_deref();
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

fn detect_fs_store(cr: &MarsService) -> Option<ArtifactStoreSpec> {
    let store_type = cr
        .spec
        .config
        .as_ref()?
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
