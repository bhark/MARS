//! Cluster-side bootstrap Job + ConfigMap synthesis and apply. Drives the
//! `ResolvedAdminDsn` + runtime-password resolution through `secrets` and
//! materialises the K8s objects that actually run `mars setup`.

use std::collections::BTreeMap;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::api::{Api, Patch, PatchParams};
use serde_json::Value as JsonValue;
use tracing::info;

use super::plan::{
    CatalogBootstrapPlan, cluster_bootstrap_configmap_name, cluster_bootstrap_job_name, cluster_labels, plan_hash,
};
use super::secrets::{ensure_runtime_password_secret, resolve_admin_dsn};
use super::{ClusterCtx, SecretKeyRef};
use crate::children::labels;
use crate::error::Result;

pub(super) async fn apply_one(
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

/// Build a one-source `mars_config::Config` payload as a JSON value so the
/// existing config canonicaliser / validator can verify it without bespoke
/// machinery. Bands carry a single throwaway entry; layers stay empty; the
/// service block is named after `<cluster>-<source>` so the Job's logs make
/// the origin obvious.
pub(crate) fn synthesise_config(plan: &CatalogBootstrapPlan) -> Result<JsonValue> {
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
