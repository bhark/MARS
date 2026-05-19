//! Cluster-side admin DSN + runtime password resolution. Mirrors the
//! per-service variant but reads its bootstrap spec out of the catalog entry
//! and writes managed Secrets owned by the cluster CR.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::api::{Api, Patch, PatchParams};
use tracing::info;

use super::plan::{
    CatalogBootstrapPlan, CatalogSourceBootstrap, cluster_bootstrap_admin_credentials_secret_name, cluster_labels,
    cluster_runtime_credentials_secret_name,
};
use super::{ClusterCtx, SecretKeyRef};
use crate::children::labels;
use crate::dsn;
use crate::error::{OperatorError, Result};

pub(super) struct ResolvedAdminDsn {
    pub(super) admin_dsn_ref: SecretKeyRef,
    pub(super) resolved_secret_resource_version: String,
}

pub(super) async fn resolve_admin_dsn(
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
            &PatchParams::apply(crate::controller::FIELD_MANAGER).force(),
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

pub(super) async fn ensure_runtime_password_secret(
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
        &PatchParams::apply(crate::controller::FIELD_MANAGER).force(),
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
