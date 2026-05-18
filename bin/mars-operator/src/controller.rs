//! Long-running operator entry point. Starts the controller, the metrics
//! server, and leader election (if enabled), then blocks on shutdown.

use std::sync::Arc;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Service, ServiceAccount};
use kube::Client;
use kube::api::Api;
use kube::runtime::Controller;
use kube::runtime::watcher::Config as WatcherConfig;
use kube_lease_manager::LeaseManagerBuilder;
use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::cli::Cli;
use crate::crd::spec::MarsService;
use crate::metrics::{self, Metrics};
use crate::reconcile::{self, Ctx};

const LEASE_NAME: &str = "mars-operator-leader";
const LEASE_DURATION_SECS: u64 = 30;
const LEASE_GRACE_SECS: u64 = 5;
const LEASE_FIELD_MANAGER: &str = "mars-operator-lease";

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

    let lease_rx = if cli.leader_elect {
        Some(start_leader_election(client.clone(), &cli.namespace).await?)
    } else {
        None
    };

    // operator vX.Y.Z always spawns mars vX.Y.Z. CARGO_PKG_VERSION is baked
    // at compile time; under the tag-driven release flow CI patches the
    // workspace version to the tag before build.
    let runtime_image = format!("{}:{}", cli.runtime_image_repository, env!("CARGO_PKG_VERSION"));
    info!(image = %runtime_image, "runtime/compiler image");

    let ctx = Arc::new(Ctx {
        client: client.clone(),
        field_manager: cli.field_manager.clone(),
        metrics: metrics_svc,
        runtime_image,
    });

    let crs: Api<MarsService> = Api::all(client.clone());
    let cms: Api<ConfigMap> = Api::all(client.clone());
    let deps: Api<Deployment> = Api::all(client.clone());
    let svcs: Api<Service> = Api::all(client.clone());
    let pvcs: Api<PersistentVolumeClaim> = Api::all(client.clone());
    let jobs: Api<Job> = Api::all(client.clone());
    let sas: Api<ServiceAccount> = Api::all(client.clone());

    let controller = Controller::new(crs, WatcherConfig::default())
        .owns(cms, WatcherConfig::default())
        .owns(deps, WatcherConfig::default())
        .owns(svcs, WatcherConfig::default())
        .owns(pvcs, WatcherConfig::default())
        .owns(jobs, WatcherConfig::default())
        .owns(sas, WatcherConfig::default())
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
        _ = wait_for_lease_loss(lease_rx) => {
            // exit so the kubelet restarts us and re-enters acquisition;
            // matches client-go / controller-runtime convention and avoids
            // a multi-leader window from a half-shut-down replica.
            error!("lost leader lease; exiting");
            std::process::exit(1);
        }
    }

    Ok(())
}

/// Build a LeaseManager, park until we hold the Lease, return the watch
/// receiver so the caller can react to loss. The renewer task spawned by
/// `manager.watch()` is detached and lives as long as the receiver does.
async fn start_leader_election(client: Client, namespace: &str) -> Result<watch::Receiver<bool>> {
    let identity = std::env::var("HOSTNAME").unwrap_or_else(|_| "mars-operator".into());

    let manager = LeaseManagerBuilder::new(client, LEASE_NAME)
        .with_namespace(namespace)
        .with_identity(&identity)
        .with_duration(LEASE_DURATION_SECS)
        .with_grace(LEASE_GRACE_SECS)
        .with_field_manager(LEASE_FIELD_MANAGER)
        .build()
        .await
        .context("build LeaseManager")?;

    let (mut rx, _task) = manager.watch().await;

    // park until first acquire
    loop {
        if *rx.borrow_and_update() {
            break;
        }
        rx.changed()
            .await
            .context("lease watch channel closed before acquire")?;
    }
    info!(identity = %identity, lease = LEASE_NAME, namespace = %namespace, "acquired leader lease");
    Ok(rx)
}

/// Resolves on the first transition leader -> non-leader, or on channel
/// close (which means the renewer task died). Pending forever when election
/// is disabled.
async fn wait_for_lease_loss(rx: Option<watch::Receiver<bool>>) {
    let Some(mut rx) = rx else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        if rx.changed().await.is_err() {
            warn!("lease watch channel closed; treating as lease loss");
            return;
        }
        if !*rx.borrow_and_update() {
            return;
        }
    }
}

#[cfg(test)]
mod tests;
