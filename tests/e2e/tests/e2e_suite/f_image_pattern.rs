//! image-pattern fill seam smoke: proves the `FillPaint::Image` path is wired
//! end-to-end (operator mounts the bitmap configmap into the compiler pod, the
//! compiler bundles it into `image_artifact`, the runtime decodes + binds it,
//! the renderer dispatches a tiled tiny-skia pattern). Pixel-level correctness
//! is owned by the parity suite.

use anyhow::{Result, anyhow};
use mars_e2e_kind::{http, wait};
use std::time::Duration;

use super::scenario::Scenario;

const PNG_MAGIC: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
const MIN_BODY_BYTES: usize = 512;

// bbox fits the synthetic pattern_zone polygon (tests/e2e/sql/synthetic-pattern.sql).
// at 256x256 the 16x16 stripe tile repeats 16 times across the frame.
const PATH_AND_QUERY: &str = "/wms?service=WMS&version=1.3.0&request=GetMap\
&layers=pattern_zone&styles=&crs=EPSG:25832\
&bbox=860000,6105000,880000,6125000&width=256&height=256&format=image/png";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn image_pattern_serves_png() -> Result<()> {
    let scenario = Scenario::up("image-pattern").await?;
    let client = scenario.client.clone();
    let ns = &scenario.ns.name;

    wait::until("runtime /readyz returns 200", Duration::from_secs(300), || async {
        let r = http::get(client.clone(), ns, "mars-e2e-runtime", 8080, "/readyz").await?;
        if r.status == 200 { Ok(Some(())) } else { Ok(None) }
    })
    .await?;

    let r = http::get(client.clone(), ns, "mars-e2e-runtime", 8080, PATH_AND_QUERY).await?;
    if r.status != 200 {
        return Err(anyhow!("image_pattern status {} ({} bytes)", r.status, r.body.len()));
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
