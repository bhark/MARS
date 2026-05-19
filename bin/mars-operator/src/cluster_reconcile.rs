//! Cluster-scoped reconciler for `MarsServiceCluster`. Owns one bootstrap Job
//! per (cluster, source) pair whose catalog entry declares a `bootstrap` block.
//! Services no longer carry `spec.bootstrap`; the cluster owns the postgres
//! catalog provisioning entirely, so Jobs run here and per-service delete
//! leaves the catalog (and its schemas) untouched.
//!
//! TODO: cluster-CR delete cascade. Owner references on the Jobs cover
//! garbage-collection, but rolling a proper teardown Job that drops the
//! provisioned slot/publication/role is a follow-up task.

use std::sync::Arc;
use std::time::{Duration, Instant};

use kube::runtime::controller::Action;
use tracing::{error, info, warn};

use crate::crd::cluster::{MarsServiceCluster, SecretKeyRef};
use crate::error::{OperatorError, Result};
use crate::metrics::Metrics;
use crate::owner::owner_reference;

mod jobs;
mod plan;
mod secrets;

use plan::plan_jobs;

/// Shared state for the cluster reconciler. Distinct from
/// `reconcile::Ctx` so cluster reconciles cannot accidentally reach for
/// MarsService-only fields (`poller`).
pub(crate) struct ClusterCtx {
    pub(crate) client: kube::Client,
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
            ctx.metrics.record_error(e.kind());
            Err(e)
        }
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
    let owner = owner_reference(cr.as_ref())?;
    let ns = ctx.operator_namespace.as_str();

    let plans = match plan_jobs(&cr) {
        Ok(p) => p,
        Err(e) => {
            // catalog parsing failed; surface and bail out. surfacing as a
            // status condition is a future enhancement.
            warn!(cluster = %cluster_name, "catalog parse: {e}");
            return Ok(Action::requeue(Duration::from_secs(60)));
        }
    };

    if plans.is_empty() {
        info!(cluster = %cluster_name, "no sourcesCatalog[].bootstrap entries; nothing to do");
        return Ok(Action::requeue(Duration::from_secs(60)));
    }

    for plan in plans {
        if let Err(e) = jobs::apply_one(&ctx, &cluster_name, ns, &plan, owner.clone()).await {
            error!(cluster = %cluster_name, source = %plan.source_id, "bootstrap apply: {e}");
        }
    }

    Ok(Action::requeue(Duration::from_secs(30)))
}

#[cfg(test)]
mod tests;
