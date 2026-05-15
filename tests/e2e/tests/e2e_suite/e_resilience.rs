//! runtime restart resilience under multi-replica: bring up 2 runtime pods,
//! capture a render, delete one pod, wait for the ReplicaSet to recreate it,
//! and assert the post-restart render matches within the existing render
//! tolerance. catches manifest-reload regressions and validates that the
//! horizontally-scaled runtime tier actually shares a consistent view of the
//! artifact store.

use anyhow::{Result, anyhow};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, ListParams};
use mars_e2e_kind::diff;
use mars_e2e_kind::{http, wait};
use std::time::Duration;

use super::scenario::{Scenario, ScenarioOptions};

const RENDER_QUERY: &str = "/wms?service=WMS&version=1.3.0&request=GetMap&layers=land,water,settlements,roads,buildings,waterways,poi&styles=&crs=EPSG:25832&bbox=536000,5210000,548000,5235000&width=512&height=512&format=image/png";
const RUNTIME_SELECTOR: &str = "app.kubernetes.io/instance=mars-e2e,app.kubernetes.io/component=runtime";
const MAX_CHANNEL_DELTA: u8 = 8;
const MAX_DIFF_RATIO: f32 = 0.02;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runtime_survives_pod_restart() -> Result<()> {
    let scenario = Scenario::up_with("resilience", ScenarioOptions { runtime_replicas: 2 }).await?;
    let client = scenario.client.clone();
    let ns = &scenario.ns.name;

    wait::deployment_ready(client.clone(), ns, "mars-e2e-runtime", Duration::from_secs(300)).await?;
    wait::until("runtime /readyz returns 200", Duration::from_secs(300), || async {
        let r = http::get(client.clone(), ns, "mars-e2e-runtime", 8080, "/readyz").await?;
        if r.status == 200 { Ok(Some(())) } else { Ok(None) }
    })
    .await?;

    let baseline = http::get(client.clone(), ns, "mars-e2e-runtime", 8080, RENDER_QUERY).await?;
    if baseline.status != 200 {
        return Err(anyhow!("baseline render status {}", baseline.status));
    }
    let baseline_bytes = baseline.body.to_vec();

    // pick any runtime pod and delete it. the ReplicaSet recreates a fresh
    // one; the new pod loads the manifest from the artifact store (its cache
    // PVC is ephemeral per pod).
    let pods: Api<Pod> = Api::namespaced((*client).clone(), ns);
    let listed = pods.list(&ListParams::default().labels(RUNTIME_SELECTOR)).await?;
    let victim = listed
        .items
        .into_iter()
        .find_map(|p| p.metadata.name)
        .ok_or_else(|| anyhow!("no runtime pods matched selector {RUNTIME_SELECTOR}"))?;
    pods.delete(&victim, &DeleteParams::default()).await?;

    // wait for the Deployment to settle back to 2 ready replicas. the
    // ephemeral cache means cold S3 fetch; tolerate a generous deadline.
    wait::deployment_ready(client.clone(), ns, "mars-e2e-runtime", Duration::from_secs(300)).await?;
    wait::until(
        "runtime /readyz returns 200 post-restart",
        Duration::from_secs(300),
        || async {
            let r = http::get(client.clone(), ns, "mars-e2e-runtime", 8080, "/readyz").await?;
            if r.status == 200 { Ok(Some(())) } else { Ok(None) }
        },
    )
    .await?;

    // re-render via the Service (load-balanced across both pods). both serve
    // the same manifest, so the output must match the baseline within the
    // standard render tolerance.
    let after = http::get(client.clone(), ns, "mars-e2e-runtime", 8080, RENDER_QUERY).await?;
    if after.status != 200 {
        return Err(anyhow!("post-restart render status {}", after.status));
    }
    let report = diff::diff_pngs(&after.body, &baseline_bytes, MAX_CHANNEL_DELTA).map_err(|e| anyhow!("diff: {e}"))?;
    if report.diff_ratio() > MAX_DIFF_RATIO {
        return Err(anyhow!(
            "post-restart render diverges from baseline: {} (max_ratio={MAX_DIFF_RATIO})",
            report
        ));
    }
    Ok(())
}
