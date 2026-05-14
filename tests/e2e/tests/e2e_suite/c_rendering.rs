//! WMS render smoke: the operator-wired deployment serves a PNG. Rendering
//! correctness against an external reference is owned by the parity suite.

use anyhow::{Result, anyhow};
use mars_e2e_kind::{http, wait};
use std::time::Duration;

use super::scenario::Scenario;

const PNG_MAGIC: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
const MIN_BODY_BYTES: usize = 1024;

const PATH_AND_QUERY: &str = "/wms?service=WMS&version=1.3.0&request=GetMap&layers=land,water,settlements,roads,buildings,waterways,poi&styles=&crs=EPSG:25832&bbox=850000,6090000,895000,6145000&width=512&height=512&format=image/png";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wms_serves_png() -> Result<()> {
    let scenario = Scenario::up("rendering").await?;
    let client = scenario.client.clone();
    let ns = &scenario.ns.name;

    wait::until("runtime /readyz returns 200", Duration::from_secs(300), || async {
        let r = http::get(client.clone(), ns, "mars-e2e-runtime", 8080, "/readyz").await?;
        if r.status == 200 { Ok(Some(())) } else { Ok(None) }
    })
    .await?;

    let r = http::get(client.clone(), ns, "mars-e2e-runtime", 8080, PATH_AND_QUERY).await?;
    if r.status != 200 {
        return Err(anyhow!("wms render status {} ({} bytes)", r.status, r.body.len()));
    }
    if !r.body.starts_with(&PNG_MAGIC) {
        let head_len = 8.min(r.body.len());
        return Err(anyhow!(
            "response is not a PNG (first {head_len} bytes: {:?})",
            &r.body[..head_len]
        ));
    }
    if r.body.len() < MIN_BODY_BYTES {
        return Err(anyhow!(
            "png body suspiciously small: {} bytes (< {MIN_BODY_BYTES})",
            r.body.len()
        ));
    }
    Ok(())
}
