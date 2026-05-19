//! Cluster-scoped reconciler for `MarsServiceCluster`. Owns one bootstrap Job
//! per (cluster, source) pair whose catalog entry declares a `bootstrap` block.
//! Services no longer carry `spec.bootstrap`; the cluster owns the postgres
//! catalog provisioning entirely, so Jobs run here and per-service delete
//! leaves the catalog (and its schemas) untouched.
//!
//! TODO: cluster-CR delete cascade. Owner references on the Jobs cover
//! garbage-collection, but rolling a proper teardown Job that drops the
//! provisioned slot/publication/role is a follow-up task.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use blake3::Hasher;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::Resource;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::Action;
use mars_config::{Source, SourceBackend};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use tracing::{error, info, warn};

use crate::children::labels;
use crate::crd::cluster::{AdminCredentialsRef, MarsServiceCluster, SecretKeyRef, TeardownPolicy};
use crate::dsn;
use crate::error::{OperatorError, Result};
use crate::metrics::Metrics;

/// Shared state for the cluster reconciler. Distinct from
/// `reconcile::Ctx` so cluster reconciles cannot accidentally reach for
/// MarsService-only fields (`poller`, `field_manager`).
pub(crate) struct ClusterCtx {
    pub(crate) client: kube::Client,
    pub(crate) field_manager: String,
    pub(crate) metrics: Metrics,
    /// `repo:version` for the bootstrap Job container.
    pub(crate) runtime_image: String,
    /// Namespace where cluster-owned Jobs + ConfigMaps live. Cluster-scoped
    /// resources cannot directly own namespaced ones across namespaces, so we
    /// pin them to the operator's namespace. Future task: surface this on the
    /// cluster CR for explicit isolation.
    pub(crate) operator_namespace: String,
}

pub(crate) async fn reconcile(
    cr: Arc<MarsServiceCluster>,
    ctx: Arc<ClusterCtx>,
) -> std::result::Result<Action, OperatorError> {
    let start = Instant::now();
    match reconcile_inner(cr, ctx.clone()).await {
        Ok(action) => {
            ctx.metrics.record("cluster_ok", start.elapsed());
            Ok(action)
        }
        Err(e) => {
            error!(error = %e, "cluster reconcile failed");
            ctx.metrics.record("cluster_error", start.elapsed());
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

pub(crate) fn error_policy(_cr: Arc<MarsServiceCluster>, error: &OperatorError, _ctx: Arc<ClusterCtx>) -> Action {
    error!(error = %error, "cluster reconcile error_policy fired");
    Action::requeue(Duration::from_secs(15))
}

async fn reconcile_inner(cr: Arc<MarsServiceCluster>, ctx: Arc<ClusterCtx>) -> Result<Action> {
    let cluster_name = cr
        .metadata
        .name
        .clone()
        .ok_or_else(|| OperatorError::MissingField("metadata.name".into()))?;
    info!(cluster = %cluster_name, "reconciling MarsServiceCluster");

    // cluster CR is cluster-scoped: it can only own namespaced objects in the
    // operator's own namespace (cross-namespace owner refs are rejected by
    // the apiserver). delete-cascade handles GC for those Jobs / ConfigMaps.
    let owner = owner_reference(&cr)?;
    let ns = ctx.operator_namespace.as_str();

    let plans = match plan_jobs(&cr) {
        Ok(p) => p,
        Err(e) => {
            // catalog parsing failed; surface and bail out. status conditions
            // are deferred to the dedicated status task (task 8).
            warn!(cluster = %cluster_name, "catalog parse: {e}");
            return Ok(Action::requeue(Duration::from_secs(60)));
        }
    };

    if plans.is_empty() {
        info!(cluster = %cluster_name, "no sourcesCatalog[].bootstrap entries; nothing to do");
        return Ok(Action::requeue(Duration::from_secs(60)));
    }

    for plan in plans {
        if let Err(e) = apply_one(&ctx, &cluster_name, ns, &plan, owner.clone()).await {
            error!(cluster = %cluster_name, source = %plan.source_id, "bootstrap apply: {e}");
        }
    }

    Ok(Action::requeue(Duration::from_secs(30)))
}

/// One bootstrap plan per catalog source entry that declares a `bootstrap`
/// block. Pure function output; no I/O.
#[derive(Debug, Clone)]
pub(crate) struct CatalogBootstrapPlan {
    pub(crate) source_id: String,
    pub(crate) cluster_name: String,
    /// The parsed source entry (postgis backend). Used to project a minimal
    /// `mars_config::Config` into the Job's mounted ConfigMap.
    pub(crate) source: Source,
    pub(crate) bootstrap: CatalogSourceBootstrap,
    /// The cluster's `artifactStore` payload, embedded in the synthetic Config
    /// so `mars setup`'s `load_and_validate` is happy.
    pub(crate) artifact_store: JsonValue,
}

/// Cluster-side bootstrap orchestration knobs sitting alongside the
/// `mars_config::Bootstrap` payload (role + schemas) inside a catalog entry's
/// `bootstrap:` block. The `enabled` toggle gates Job creation.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CatalogSourceBootstrap {
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) admin_secret_ref: Option<SecretKeyRef>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) admin_credentials_ref: Option<AdminCredentialsRef>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) runtime_password_secret_ref: Option<SecretKeyRef>,

    #[serde(default)]
    pub(crate) teardown_on_delete: TeardownPolicy,
}

fn default_true() -> bool {
    true
}

/// Per-catalog-entry job planner. Pure: parses the cluster CR's catalog,
/// emits one `CatalogBootstrapPlan` per entry whose `bootstrap` block is set.
/// Entries without a `bootstrap` block are skipped silently. Entries that
/// fail to deserialise into `mars_config::Source` are skipped with a warning
/// trace — surfacing this as a status condition is task 8.
pub(crate) fn plan_jobs(cr: &MarsServiceCluster) -> Result<Vec<CatalogBootstrapPlan>> {
    let cluster_name = cr
        .metadata
        .name
        .clone()
        .ok_or_else(|| OperatorError::MissingField("metadata.name".into()))?;
    let mut out = Vec::new();
    for (i, entry) in cr.spec.sources_catalog.iter().enumerate() {
        // catalog entries without a bootstrap block are unconfigured for
        // provisioning; skip silently.
        let bootstrap_val = match entry.get("bootstrap") {
            Some(v) if !v.is_null() => v,
            _ => continue,
        };
        let source = match serde_json::from_value::<Source>(entry.clone()) {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    index = i,
                    "catalog entry does not deserialise into mars_config::Source: {e}"
                );
                continue;
            }
        };
        // only postgis sources support bootstrap today
        if !matches!(source.backend, SourceBackend::Postgis(_)) {
            warn!(
                source = %source.id.as_str(),
                "catalog bootstrap is only supported on postgis sources; skipping"
            );
            continue;
        }
        let catalog_bs: CatalogSourceBootstrap = match serde_json::from_value(bootstrap_val.clone()) {
            Ok(b) => b,
            Err(e) => {
                warn!(source = %source.id.as_str(), "bootstrap block does not deserialise: {e}");
                continue;
            }
        };
        out.push(CatalogBootstrapPlan {
            source_id: source.id.as_str().to_string(),
            cluster_name: cluster_name.clone(),
            source,
            bootstrap: catalog_bs,
            artifact_store: cr.spec.artifact_store.clone(),
        });
    }
    Ok(out)
}

async fn apply_one(
    ctx: &ClusterCtx,
    cluster_name: &str,
    ns: &str,
    plan: &CatalogBootstrapPlan,
    owner: OwnerReference,
) -> Result<()> {
    if !plan.bootstrap.enabled {
        info!(
            cluster = %cluster_name, source = %plan.source_id,
            "bootstrap.enabled=false; skipping Job creation"
        );
        return Ok(());
    }

    // resolve admin DSN + runtime password (mirrors per-service flow). the
    // runtime password Secret lives in the operator namespace so cluster Jobs
    // can mount it; the cluster owns it for cascade GC.
    let secret_api: Api<Secret> = Api::namespaced(ctx.client.clone(), ns);
    let resolved_admin = resolve_admin_dsn(ctx, &secret_api, cluster_name, ns, plan, owner.clone()).await?;
    let runtime_password_ref =
        ensure_runtime_password_secret(ctx, cluster_name, &plan.source_id, ns, &plan.bootstrap, owner.clone()).await?;
    let runtime_rv = secret_api
        .get_opt(&runtime_password_ref.name)
        .await?
        .and_then(|s| s.metadata.resource_version.clone())
        .unwrap_or_default();

    // synthesise the mini-Config the Job mounts; ConfigMap name embeds the
    // source id so two catalog entries cannot collide.
    let synthetic_cfg = synthesise_config(plan)?;
    let cm = build_configmap(cluster_name, &plan.source_id, ns, &synthetic_cfg, owner.clone())?;
    let cm_api: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), ns);
    cm_api
        .patch(
            cm.metadata.name.as_deref().unwrap_or(""),
            &PatchParams::apply(&ctx.field_manager).force(),
            &Patch::Apply(&cm),
        )
        .await?;

    // build + apply the Job. Idempotent: existing Complete Jobs are left alone.
    let hash = plan_hash(
        plan,
        &resolved_admin.admin_dsn_ref,
        &resolved_admin.resolved_secret_resource_version,
        &runtime_password_ref,
        &runtime_rv,
    );
    let job_name = cluster_bootstrap_job_name(cluster_name, &plan.source_id, &hash);
    let job_api: Api<Job> = Api::namespaced(ctx.client.clone(), ns);

    if let Some(existing) = job_api.get_opt(&job_name).await? {
        let succeeded = existing.status.as_ref().and_then(|s| s.succeeded).unwrap_or(0);
        if succeeded >= 1 {
            info!(cluster = %cluster_name, source = %plan.source_id, job = %job_name, "bootstrap Job already Complete");
            return Ok(());
        }
        // either still running or in retry-backoff; let it be.
        return Ok(());
    }

    let job = build_job(
        cluster_name,
        &plan.source_id,
        ns,
        &job_name,
        &cm.metadata.name.clone().unwrap_or_default(),
        &ctx.runtime_image,
        &resolved_admin.admin_dsn_ref,
        &runtime_password_ref,
        owner,
    );
    job_api
        .patch(
            &job_name,
            &PatchParams::apply(&ctx.field_manager).force(),
            &Patch::Apply(&job),
        )
        .await?;
    info!(cluster = %cluster_name, source = %plan.source_id, job = %job_name, "bootstrap Job created");
    Ok(())
}

/// Cluster-side admin DSN resolution. Mirrors the per-service variant but
/// pulls the bootstrap spec out of the catalog entry rather than the
/// MarsService spec.
struct ResolvedAdminDsn {
    admin_dsn_ref: SecretKeyRef,
    resolved_secret_resource_version: String,
}

async fn resolve_admin_dsn(
    ctx: &ClusterCtx,
    secret_api: &Api<Secret>,
    cluster_name: &str,
    ns: &str,
    plan: &CatalogBootstrapPlan,
    owner: OwnerReference,
) -> Result<ResolvedAdminDsn> {
    let bs = &plan.bootstrap;
    match (&bs.admin_secret_ref, &bs.admin_credentials_ref) {
        (Some(_), Some(_)) => Err(OperatorError::ConfigInvalid(format!(
            "sourcesCatalog[id={}].bootstrap: adminSecretRef and adminCredentialsRef are mutually exclusive",
            plan.source_id
        ))),
        (None, None) => Err(OperatorError::ConfigInvalid(format!(
            "sourcesCatalog[id={}].bootstrap: adminSecretRef or adminCredentialsRef is required when enabled",
            plan.source_id
        ))),
        (Some(r), None) => {
            let rv = secret_api
                .get_opt(&r.name)
                .await?
                .and_then(|s| s.metadata.resource_version.clone())
                .unwrap_or_default();
            Ok(ResolvedAdminDsn {
                admin_dsn_ref: r.clone(),
                resolved_secret_resource_version: rv,
            })
        }
        (None, Some(creds)) => {
            let secret = secret_api.get_opt(&creds.secret_name).await?.ok_or_else(|| {
                OperatorError::ConfigInvalid(format!(
                    "sourcesCatalog[id={}].bootstrap.adminCredentialsRef.secretName='{}' not found in namespace {ns}",
                    plan.source_id, creds.secret_name
                ))
            })?;
            let data: BTreeMap<String, Vec<u8>> = secret
                .data
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| (k, v.0))
                .collect();
            let fallback_dsn = plan.source.postgis().map(|pg| pg.dsn.as_str()).unwrap_or_default();
            let fallback = dsn::parse_dsn_components(fallback_dsn);
            let composed = dsn::compose_admin_dsn(creds, &data, &fallback)
                .map_err(|e| OperatorError::ConfigInvalid(format!("compose admin DSN: {e}")))?;
            let (admin_dsn_ref, rv) =
                ensure_managed_admin_secret(ctx, cluster_name, &plan.source_id, ns, &composed, owner).await?;
            Ok(ResolvedAdminDsn {
                admin_dsn_ref,
                resolved_secret_resource_version: rv,
            })
        }
    }
}

async fn ensure_managed_admin_secret(
    ctx: &ClusterCtx,
    cluster_name: &str,
    source_id: &str,
    ns: &str,
    composed_dsn: &str,
    owner: OwnerReference,
) -> Result<(SecretKeyRef, String)> {
    use k8s_openapi::ByteString;
    let name = cluster_bootstrap_admin_credentials_secret_name(cluster_name, source_id);
    let mut data = BTreeMap::new();
    data.insert(
        labels::BOOTSTRAP_ADMIN_DSN_KEY.to_string(),
        ByteString(composed_dsn.as_bytes().to_vec()),
    );
    let secret = Secret {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            namespace: Some(ns.into()),
            labels: Some(cluster_labels(cluster_name, source_id, "bootstrap-admin-credentials")),
            owner_references: Some(vec![owner]),
            ..Default::default()
        },
        data: Some(data),
        type_: Some("Opaque".into()),
        ..Default::default()
    };
    let api: Api<Secret> = Api::namespaced(ctx.client.clone(), ns);
    let patched = api
        .patch(
            &name,
            &PatchParams::apply(&ctx.field_manager).force(),
            &Patch::Apply(&secret),
        )
        .await?;
    let rv = patched.metadata.resource_version.unwrap_or_default();
    Ok((
        SecretKeyRef {
            name,
            key: labels::BOOTSTRAP_ADMIN_DSN_KEY.to_string(),
        },
        rv,
    ))
}

async fn ensure_runtime_password_secret(
    ctx: &ClusterCtx,
    cluster_name: &str,
    source_id: &str,
    ns: &str,
    bs: &CatalogSourceBootstrap,
    owner: OwnerReference,
) -> Result<SecretKeyRef> {
    if let Some(byo) = &bs.runtime_password_secret_ref {
        return Ok(byo.clone());
    }
    let name = cluster_runtime_credentials_secret_name(cluster_name, source_id);
    let api: Api<Secret> = Api::namespaced(ctx.client.clone(), ns);

    if api.get_opt(&name).await?.is_some() {
        return Ok(SecretKeyRef {
            name,
            key: labels::RUNTIME_PASSWORD_KEY.to_string(),
        });
    }

    let password = generate_runtime_password();
    use k8s_openapi::ByteString;
    let mut data = BTreeMap::new();
    data.insert(
        labels::RUNTIME_PASSWORD_KEY.to_string(),
        ByteString(password.as_bytes().to_vec()),
    );
    let secret = Secret {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            namespace: Some(ns.into()),
            labels: Some(cluster_labels(cluster_name, source_id, "runtime-credentials")),
            owner_references: Some(vec![owner]),
            ..Default::default()
        },
        data: Some(data),
        type_: Some("Opaque".into()),
        ..Default::default()
    };
    api.patch(
        &name,
        &PatchParams::apply(&ctx.field_manager).force(),
        &Patch::Apply(&secret),
    )
    .await?;
    info!(cluster = %cluster_name, source = %source_id, secret = %name, "generated operator-managed runtime password");
    Ok(SecretKeyRef {
        name,
        key: labels::RUNTIME_PASSWORD_KEY.to_string(),
    })
}

fn generate_runtime_password() -> String {
    use rand::RngExt;
    use rand::distr::Alphanumeric;
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

/// Build a one-source `mars_config::Config` payload as a JSON value so the
/// existing config canonicaliser / validator can verify it without bespoke
/// machinery. Bands carry a single throwaway entry; layers stay empty; the
/// service block is named after `<cluster>-<source>` so the Job's logs make
/// the origin obvious.
fn synthesise_config(plan: &CatalogBootstrapPlan) -> Result<JsonValue> {
    let source_value = serde_json::to_value(&plan.source)?;
    let cfg = serde_json::json!({
        "service": { "name": format!("{}-{}-bootstrap", plan.cluster_name, plan.source_id) },
        "scales": {
            "bands": [
                { "name": "bootstrap", "max_denom_exclusive": u32::MAX }
            ]
        },
        "interfaces": {},
        "sources": [source_value],
        "artifacts": plan.artifact_store,
    });
    // structural validation: keeps a malformed catalog from rolling a
    // crashlooping Job. mirrors the per-service validate flow.
    crate::config::validate(&cfg)?;
    Ok(cfg)
}

fn build_configmap(
    cluster_name: &str,
    source_id: &str,
    ns: &str,
    config: &JsonValue,
    owner: OwnerReference,
) -> Result<ConfigMap> {
    let yaml = crate::config::canonicalize_yaml(config)?;
    let mut data: BTreeMap<String, String> = BTreeMap::new();
    data.insert("mars.yaml".into(), yaml);
    Ok(ConfigMap {
        metadata: ObjectMeta {
            name: Some(cluster_bootstrap_configmap_name(cluster_name, source_id)),
            namespace: Some(ns.into()),
            labels: Some(cluster_labels(cluster_name, source_id, "config")),
            owner_references: Some(vec![owner]),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    })
}

#[allow(clippy::too_many_arguments)]
fn build_job(
    cluster_name: &str,
    source_id: &str,
    ns: &str,
    job_name: &str,
    config_map_name: &str,
    image: &str,
    admin_dsn_ref: &SecretKeyRef,
    runtime_password_ref: &SecretKeyRef,
    owner: OwnerReference,
) -> Job {
    use k8s_openapi::api::batch::v1::JobSpec;
    use k8s_openapi::api::core::v1::{
        ConfigMapVolumeSource, Container, EnvVar, EnvVarSource, PodSpec, PodTemplateSpec, SecretKeySelector, Volume,
        VolumeMount,
    };

    let labels_map = cluster_labels(cluster_name, source_id, labels::COMPONENT_BOOTSTRAP);
    let container = Container {
        name: "bootstrap".into(),
        image: Some(image.into()),
        args: Some(vec!["setup".into(), "--config".into(), "/etc/mars/mars.yaml".into()]),
        env: Some(vec![
            EnvVar {
                name: "MARS_ADMIN_DSN".into(),
                value_from: Some(EnvVarSource {
                    secret_key_ref: Some(SecretKeySelector {
                        name: admin_dsn_ref.name.clone(),
                        key: admin_dsn_ref.key.clone(),
                        optional: Some(false),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            EnvVar {
                name: "MARS_RUNTIME_PASSWORD".into(),
                value_from: Some(EnvVarSource {
                    secret_key_ref: Some(SecretKeySelector {
                        name: runtime_password_ref.name.clone(),
                        key: runtime_password_ref.key.clone(),
                        optional: Some(false),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ]),
        volume_mounts: Some(vec![VolumeMount {
            name: "config".into(),
            mount_path: "/etc/mars/mars.yaml".into(),
            sub_path: Some("mars.yaml".into()),
            read_only: Some(true),
            ..Default::default()
        }]),
        security_context: Some(crate::children::compiler::container_security_context()),
        ..Default::default()
    };

    Job {
        metadata: ObjectMeta {
            name: Some(job_name.into()),
            namespace: Some(ns.into()),
            labels: Some(labels_map.clone()),
            owner_references: Some(vec![owner]),
            ..Default::default()
        },
        spec: Some(JobSpec {
            backoff_limit: Some(3),
            ttl_seconds_after_finished: Some(86_400),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels_map),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    restart_policy: Some("Never".into()),
                    security_context: Some(crate::children::compiler::pod_security_context()),
                    containers: vec![container],
                    volumes: Some(vec![Volume {
                        name: "config".into(),
                        config_map: Some(ConfigMapVolumeSource {
                            name: config_map_name.into(),
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

fn plan_hash(
    plan: &CatalogBootstrapPlan,
    admin_dsn_ref: &SecretKeyRef,
    admin_rv: &str,
    runtime_ref: &SecretKeyRef,
    runtime_rv: &str,
) -> String {
    let mut h = Hasher::new();
    h.update(plan.cluster_name.as_bytes());
    h.update(b"|");
    h.update(plan.source_id.as_bytes());
    h.update(b"|");
    if let Some(pg) = plan.source.postgis() {
        if let Some(bs) = &pg.bootstrap {
            h.update(bs.role.as_bytes());
            h.update(b"|");
            let mut schemas = bs.schemas.clone();
            schemas.sort();
            for s in &schemas {
                h.update(s.as_bytes());
                h.update(b",");
            }
        }
        if let Some(cf) = &pg.change_feed {
            h.update(b"|");
            h.update(cf.publication.as_deref().unwrap_or("").as_bytes());
            h.update(b"|");
            h.update(cf.slot.as_deref().unwrap_or("").as_bytes());
        }
    }
    h.update(b"|");
    h.update(admin_dsn_ref.name.as_bytes());
    h.update(b":");
    h.update(admin_dsn_ref.key.as_bytes());
    h.update(b"|");
    h.update(admin_rv.as_bytes());
    h.update(b"|");
    h.update(runtime_ref.name.as_bytes());
    h.update(b":");
    h.update(runtime_ref.key.as_bytes());
    h.update(b"|");
    h.update(runtime_rv.as_bytes());
    let digest = h.finalize();
    digest.to_hex().as_str()[..10].to_string()
}

pub(crate) fn owner_reference(cr: &MarsServiceCluster) -> Result<OwnerReference> {
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
        api_version: MarsServiceCluster::api_version(&()).into_owned(),
        kind: MarsServiceCluster::kind(&()).into_owned(),
        name,
        uid,
        controller: Some(true),
        block_owner_deletion: Some(true),
    })
}

pub(crate) fn cluster_bootstrap_job_name(cluster: &str, source_id: &str, hash: &str) -> String {
    format!("{cluster}-bootstrap-{source_id}-{hash}")
}

pub(crate) fn cluster_bootstrap_configmap_name(cluster: &str, source_id: &str) -> String {
    format!("{cluster}-bootstrap-{source_id}-config")
}

pub(crate) fn cluster_runtime_credentials_secret_name(cluster: &str, source_id: &str) -> String {
    format!("{cluster}-{source_id}-runtime-credentials")
}

pub(crate) fn cluster_bootstrap_admin_credentials_secret_name(cluster: &str, source_id: &str) -> String {
    format!("{cluster}-{source_id}-bootstrap-admin-credentials")
}

fn cluster_labels(cluster: &str, source_id: &str, component: &str) -> BTreeMap<String, String> {
    let mut m = labels::labels(cluster, component);
    m.insert("mars.forn.dk/cluster".into(), cluster.into());
    m.insert("mars.forn.dk/source".into(), source_id.into());
    m
}

#[cfg(test)]
mod tests;
