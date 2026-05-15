//! incremental compile cycle: mutate the source DB after bootstrap, wait for
//! the compiler to publish a new manifest, then re-render and assert the
//! output reflects the change. proves the change-feed -> compile -> publish ->
//! manifest-swap chain works in-cluster, end-to-end. the library-level
//! integration test in `bin/mars/tests/integration_compiler_cycle.rs` already
//! covers the same path against testcontainers, so this scenario is
//! responsible only for the Deployment-topology wiring (separate compiler /
//! runtime pods, S3 store, operator-rendered ConfigMap).

use anyhow::{Context, Result, anyhow};
use mars_e2e_kind::{deploy, http, metrics, wait};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use super::scenario::Scenario;

const RENDER_QUERY: &str = "/wms?service=WMS&version=1.3.0&request=GetMap&layers=land,water,settlements,roads,buildings,waterways,poi&styles=&crs=EPSG:25832&bbox=536000,5210000,548000,5235000&width=512&height=512&format=image/png";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incremental_cycle_propagates() -> Result<()> {
    let scenario = Scenario::up("incremental").await?;
    let client = scenario.client.clone();
    let ns = &scenario.ns.name;

    wait::until("runtime /readyz returns 200", Duration::from_secs(300), || async {
        let r = http::get(client.clone(), ns, "mars-e2e-runtime", 8080, "/readyz").await?;
        if r.status == 200 { Ok(Some(())) } else { Ok(None) }
    })
    .await?;

    // baseline manifest version + render
    let baseline_version = manifest_version(client.clone(), ns).await?;
    let baseline = http::get(client.clone(), ns, "mars-e2e-runtime", 8080, RENDER_QUERY).await?;
    if baseline.status != 200 {
        return Err(anyhow!("baseline render status {}", baseline.status));
    }
    let baseline_bytes = baseline.body.to_vec();

    // apply the mutate-source Job. it ships the SQL via the same fixture-sql
    // ConfigMap built by the scenario; the Job container is short-lived.
    let disc = deploy::discovery(client.clone()).await?;
    let mtmpl = manifests_dir();
    deploy::apply_template(
        client.clone(),
        &disc,
        ns,
        mtmpl.join("mutate-source.yaml.tmpl"),
        &HashMap::new(),
    )
    .await
    .context("apply mutate-source manifest")?;
    wait::job_succeeded(client.clone(), ns, "mutate-source", Duration::from_secs(120)).await?;

    // wait for the compiler to capture the change, publish a new manifest, and
    // for the runtime to swap it in. the default cycle window is in single-digit
    // seconds so 300s is generous; covers cold replication slot drains.
    wait::until(
        "mars_manifest_version advances past baseline",
        Duration::from_secs(300),
        || async {
            let v = manifest_version(client.clone(), ns).await?;
            if v > baseline_version { Ok(Some(v)) } else { Ok(None) }
        },
    )
    .await?;

    // re-render. mutate-source.sql edits e2e_source.settlements (a `mid`-band
    // layer) inside the bbox; at the request's scale_denom (~88k) the poi
    // layer's `hi`-band source is out of window, so changes there would be
    // invisible. settlements changes produce a clear bytewise diff.
    let after = http::get(client.clone(), ns, "mars-e2e-runtime", 8080, RENDER_QUERY).await?;
    if after.status != 200 {
        return Err(anyhow!("post-edit render status {}", after.status));
    }
    if after.body.as_ref() == baseline_bytes.as_slice() {
        return Err(anyhow!(
            "post-edit render is bytewise identical to baseline ({} bytes)",
            baseline_bytes.len()
        ));
    }
    Ok(())
}

async fn manifest_version(client: std::sync::Arc<kube::Client>, ns: &str) -> Result<u64> {
    let r = http::get(client, ns, "mars-e2e-runtime", 8080, "/metrics").await?;
    if r.status != 200 {
        return Err(anyhow!("/metrics status {}", r.status));
    }
    let scraped = metrics::Scraped::parse(&r.body)?;
    let v = scraped.gauge("mars_manifest_version")?;
    if !v.is_finite() || v < 0.0 {
        return Err(anyhow!("mars_manifest_version not a sane gauge: {v}"));
    }
    Ok(v as u64)
}

fn manifests_dir() -> PathBuf {
    std::env::current_dir()
        .ok()
        .map(|p| p.join("manifests"))
        .unwrap_or_else(|| PathBuf::from("manifests"))
}
