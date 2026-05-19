//! Pure planning + naming for cluster-scoped bootstrap Jobs. No I/O: takes a
//! parsed `MarsServiceCluster`, emits one `CatalogBootstrapPlan` per catalog
//! entry that declares a `bootstrap` block.

use std::collections::BTreeMap;

use blake3::Hasher;
use mars_config::{Source, SourceBackend};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use tracing::warn;

use crate::children::labels;
use crate::crd::cluster::{AdminCredentialsRef, MarsServiceCluster, SecretKeyRef, TeardownPolicy};
use crate::error::{OperatorError, Result};

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
/// trace — a future status condition could surface this on the CR.
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

pub(crate) fn plan_hash(
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

pub(super) fn cluster_labels(cluster: &str, source_id: &str, component: &str) -> BTreeMap<String, String> {
    let mut m = labels::labels(cluster, component);
    m.insert("mars.forn.dk/cluster".into(), cluster.into());
    m.insert("mars.forn.dk/source".into(), source_id.into());
    m
}
