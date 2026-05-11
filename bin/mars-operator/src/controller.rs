//! Long-running operator entry point. Starts the controller, the metrics
//! server, and a leader-election loop (if enabled), then blocks on shutdown.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Service};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Client;
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::runtime::Controller;
use kube::runtime::watcher::Config as WatcherConfig;
use tracing::{error, info, warn};

use crate::cli::Cli;
use crate::crd::MarsService;
use crate::metrics::{self, Metrics};
use crate::reconcile::{self, Ctx};

const LEASE_NAME: &str = "mars-operator-leader";
const LEASE_DURATION_SECS: i32 = 30;
const LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(10);
const LEASE_RETRY_INTERVAL: Duration = Duration::from_secs(5);

pub(crate) async fn run(cli: Cli) -> Result<()> {
    let client = Client::try_default().await.context("kube client")?;

    let metrics_svc = Metrics::new().context("metrics registry")?;

    let metrics_serve = {
        let m = metrics_svc.clone();
        let metrics_addr = cli.metrics_addr;
        let health_addr = cli.health_addr;
        tokio::spawn(async move {
            if let Err(e) = metrics::serve(m, metrics_addr, health_addr).await {
                error!(error = %e, "metrics/health server exited");
            }
        })
    };

    if cli.leader_elect {
        acquire_lease(client.clone(), &cli.namespace).await?;
    }

    let ctx = Arc::new(Ctx {
        client: client.clone(),
        field_manager: cli.field_manager.clone(),
        metrics: metrics_svc,
    });

    let crs: Api<MarsService> = Api::all(client.clone());
    let cms: Api<ConfigMap> = Api::all(client.clone());
    let deps: Api<Deployment> = Api::all(client.clone());
    let svcs: Api<Service> = Api::all(client.clone());
    let pvcs: Api<PersistentVolumeClaim> = Api::all(client.clone());

    let controller = Controller::new(crs, WatcherConfig::default())
        .owns(cms, WatcherConfig::default())
        .owns(deps, WatcherConfig::default())
        .owns(svcs, WatcherConfig::default())
        .owns(pvcs, WatcherConfig::default())
        .shutdown_on_signal()
        .run(reconcile::reconcile, reconcile::error_policy, ctx)
        .for_each(|res| async move {
            match res {
                Ok((obj, _)) => info!(name = %obj.name, "reconciled"),
                Err(e) => error!(error = %e, "reconcile loop error"),
            }
        });

    tokio::select! {
        _ = controller => {
            info!("controller exited");
        }
        _ = metrics_serve => {
            info!("metrics server exited");
        }
    }

    Ok(())
}

/// Acquire (and start renewing) the operator leader lease. Blocks until
/// leadership is held - other replicas park here. If the API call fails
/// transiently we retry; permanent RBAC failures bubble up.
async fn acquire_lease(client: Client, namespace: &str) -> Result<()> {
    let identity = std::env::var("HOSTNAME").unwrap_or_else(|_| "mars-operator".into());
    let api: Api<Lease> = Api::namespaced(client, namespace);

    loop {
        match try_acquire(&api, &identity).await {
            Ok(true) => {
                info!(identity = %identity, "acquired leader lease");
                spawn_renewer(api, identity);
                return Ok(());
            }
            Ok(false) => {
                tokio::time::sleep(LEASE_RETRY_INTERVAL).await;
            }
            Err(e) => {
                warn!(error = %e, "lease acquisition error, retrying");
                tokio::time::sleep(LEASE_RETRY_INTERVAL).await;
            }
        }
    }
}

async fn try_acquire(api: &Api<Lease>, identity: &str) -> Result<bool> {
    let now = k8s_openapi::jiff::Timestamp::now();
    let desired = Lease {
        metadata: ObjectMeta {
            name: Some(LEASE_NAME.into()),
            ..Default::default()
        },
        spec: Some(LeaseSpec {
            holder_identity: Some(identity.into()),
            lease_duration_seconds: Some(LEASE_DURATION_SECS),
            acquire_time: Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime(now)),
            renew_time: Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime(now)),
            ..Default::default()
        }),
    };
    match api.create(&PostParams::default(), &desired).await {
        Ok(_) => Ok(true),
        Err(kube::Error::Api(e)) if e.code == 409 => {
            let existing = api.get(LEASE_NAME).await?;
            if existing.spec.as_ref().and_then(|s| s.holder_identity.as_deref()) == Some(identity)
                || lease_expired(&existing)
            {
                let patch = serde_json::json!({
                    "spec": {
                        "holderIdentity": identity,
                        "leaseDurationSeconds": LEASE_DURATION_SECS,
                        "renewTime": format_micro(now),
                    }
                });
                api.patch(LEASE_NAME, &PatchParams::default(), &Patch::Merge(&patch))
                    .await?;
                return Ok(true);
            }
            Ok(false)
        }
        Err(e) => Err(e.into()),
    }
}

fn lease_expired(lease: &Lease) -> bool {
    let Some(spec) = &lease.spec else {
        return true;
    };
    let dur = spec.lease_duration_seconds.unwrap_or(LEASE_DURATION_SECS) as i64;
    let Some(renew) = &spec.renew_time else {
        return true;
    };
    let now = k8s_openapi::jiff::Timestamp::now();
    (now.as_second() - renew.0.as_second()) > dur
}

fn format_micro(t: k8s_openapi::jiff::Timestamp) -> String {
    // MicroTime is wire-encoded as RFC3339 with microsecond precision; jiff's
    // default Display format already round-trips that exactly.
    t.to_string()
}

fn spawn_renewer(api: Api<Lease>, identity: String) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(LEASE_RENEW_INTERVAL).await;
            let now = k8s_openapi::jiff::Timestamp::now();
            let patch = serde_json::json!({
                "spec": {
                    "holderIdentity": identity,
                    "renewTime": format_micro(now),
                }
            });
            if let Err(e) = api
                .patch(LEASE_NAME, &PatchParams::default(), &Patch::Merge(&patch))
                .await
            {
                warn!(error = %e, "lease renewal failed");
            }
        }
    });
}
