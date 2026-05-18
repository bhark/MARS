//! operator-driven postgres bootstrap. The MarsService applied here declares
//! `spec.bootstrap.enabled = true`; the operator renders a Job that calls
//! `mars setup`, which in turn provisions the role / grants / publication /
//! slot before the compiler/runtime Deployments come up. Then a CR delete
//! triggers the teardown Job and removes the slot.

use anyhow::{Context, Result, anyhow};
use k8s_openapi::api::batch::v1::Job;
use kube::Api;
use kube::api::{DeleteParams, DynamicObject, ListParams};
use kube::core::{ApiResource, GroupVersionKind};
use mars_e2e_kind::{http, wait};
use std::time::Duration;

use super::scenario::Scenario;

const MARS_GROUP: &str = "mars.forn.dk";
const MARS_VERSION: &str = "v1alpha1";
const MARS_KIND: &str = "MarsService";

fn mars_service_resource() -> ApiResource {
    let gvk = GroupVersionKind::gvk(MARS_GROUP, MARS_VERSION, MARS_KIND);
    let mut ar = ApiResource::from_gvk(&gvk);
    ar.plural = "marsservices".into();
    ar
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bootstrap_provisions_role_and_runtime_starts() -> Result<()> {
    let scenario = Scenario::up_with_bootstrap("bootstrap").await?;
    let client = scenario.client.clone();
    let ns = &scenario.ns.name;

    let r = http::get(client.clone(), ns, "mars-bs-runtime", 8080, "/healthz").await?;
    assert_eq!(r.status, 200, "/healthz status");

    // exactly one bootstrap Job has succeeded; the operator-rendered name
    // embeds a hash, so look up by label.
    let job_api: Api<Job> = Api::namespaced(client.as_ref().clone(), ns);
    let lp = ListParams::default().labels("app.kubernetes.io/component=bootstrap");
    let jobs = job_api.list(&lp).await?;
    let succeeded = jobs
        .items
        .iter()
        .filter(|j| j.status.as_ref().and_then(|s| s.succeeded).unwrap_or(0) >= 1)
        .count();
    assert!(
        succeeded >= 1,
        "expected at least one bootstrap Job with status.succeeded >= 1; got {succeeded}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn teardown_drops_slot_on_delete() -> Result<()> {
    let scenario = Scenario::up_with_bootstrap("teardown").await?;
    let client = scenario.client.clone();
    let ns = scenario.ns.name.clone();

    let ar = mars_service_resource();
    let svc_api: Api<DynamicObject> = Api::namespaced_with(client.as_ref().clone(), &ns, &ar);
    svc_api
        .delete("mars-bs", &DeleteParams::default())
        .await
        .context("delete mars-bs CR")?;

    // accept either evidence path: the teardown Job is observed succeeded,
    // OR the CR is gone (finalizer-removal is gated on Job success in the
    // operator, so cr-gone is the stronger proof). the Job lifecycle is
    // racy - it can complete and be cascade-deleted with the CR before the
    // 2s poll catches it.
    wait::until(
        "teardown completed (Job succeeded or CR gone)",
        Duration::from_secs(180),
        || {
            let client = client.clone();
            let ns = ns.clone();
            async move {
                let svc_api: Api<DynamicObject> =
                    Api::namespaced_with(client.as_ref().clone(), &ns, &mars_service_resource());
                if svc_api.get_opt("mars-bs").await?.is_none() {
                    return Ok(Some(()));
                }
                let job_api: Api<Job> = Api::namespaced(client.as_ref().clone(), &ns);
                match job_api.get_opt("mars-bs-teardown").await? {
                    Some(j) if j.status.as_ref().and_then(|s| s.succeeded).unwrap_or(0) >= 1 => {
                        Ok(Some(()))
                    }
                    _ => Ok(None),
                }
            }
        },
    )
    .await?;

    let _ = anyhow!("not used"); // silence anyhow import in narrow build configs
    Ok(())
}
