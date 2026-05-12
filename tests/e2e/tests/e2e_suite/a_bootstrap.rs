//! cluster + operator + MarsService come up. health endpoints green. manifest
//! has been published (mars_manifest_version > 0).

use anyhow::Result;
use mars_e2e_kind::{http, metrics, wait};
use std::time::Duration;

use super::scenario::Scenario;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bootstrap() -> Result<()> {
    let scenario = Scenario::up("bootstrap").await?;
    let client = scenario.client.clone();
    let ns = &scenario.ns.name;

    // operator-rendered Deployments + Service: names follow the child-builder
    // convention `{svc}-{role}` (see bin/mars-operator/src/children/labels.rs).
    // update if the operator changes its naming.
    wait::deployment_ready(client.clone(), ns, "mars-e2e-runtime", Duration::from_secs(300)).await?;
    wait::deployment_ready(client.clone(), ns, "mars-e2e-compiler", Duration::from_secs(300)).await?;

    // /healthz: always 200 once the http server is listening.
    let r = http::get(client.clone(), ns, "mars-e2e-runtime", 8080, "/healthz").await?;
    assert_eq!(r.status, 200, "/healthz status");

    // /readyz: 200 once a manifest has been loaded from the artifact store.
    wait::until("runtime /readyz returns 200", Duration::from_secs(300), || async {
        let r = http::get(client.clone(), ns, "mars-e2e-runtime", 8080, "/readyz").await?;
        if r.status == 200 { Ok(Some(())) } else { Ok(None) }
    })
    .await?;

    // /metrics: parse + assert manifest version published, no rejects.
    let r = http::get(client.clone(), ns, "mars-e2e-runtime", 8080, "/metrics").await?;
    assert_eq!(r.status, 200);
    let scraped = metrics::Scraped::parse(&r.body)?;
    let version = scraped.gauge("mars_manifest_version")?;
    assert!(version > 0.0, "mars_manifest_version must be > 0; got {version}");
    let rejects = scraped.sum("mars_manifest_reject_total").unwrap_or(0.0);
    assert_eq!(rejects, 0.0, "mars_manifest_reject_total must be 0; got {rejects}");

    Ok(())
}
