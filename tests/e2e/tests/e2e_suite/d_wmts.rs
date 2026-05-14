//! WMTS happy-path: GetCapabilities advertises every layer + the configured
//! TileMatrixSet, and one REST GetTile returns a valid PNG. proves the second
//! interface is actually wired (the bootstrap and rendering scenarios only
//! exercise WMS).

use anyhow::{Result, anyhow};
use mars_e2e_kind::{http, wait};
use std::time::Duration;

use super::scenario::Scenario;

const LAYERS: &[&str] = &["land", "water", "settlements", "roads", "buildings", "waterways", "poi"];
const TMS_ID: &str = "e2e_25832";

const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wmts_capabilities_and_tile() -> Result<()> {
    let scenario = Scenario::up("wmts").await?;
    let client = scenario.client.clone();
    let ns = &scenario.ns.name;

    wait::until("runtime /readyz returns 200", Duration::from_secs(300), || async {
        let r = http::get(client.clone(), ns, "mars-e2e-runtime", 8080, "/readyz").await?;
        if r.status == 200 { Ok(Some(())) } else { Ok(None) }
    })
    .await?;

    // GetCapabilities: assert 200 + every layer identifier + the TMS id show up
    // somewhere in the XML body. mirrors the substring-match style used by the
    // in-process tests in crates/interfaces/mars-wmts/src/capabilities.rs.
    let caps = http::get(
        client.clone(),
        ns,
        "mars-e2e-runtime",
        8080,
        "/wmts?service=WMTS&version=1.0.0&request=GetCapabilities",
    )
    .await?;
    if caps.status != 200 {
        return Err(anyhow!("GetCapabilities status {}", caps.status));
    }
    let body = std::str::from_utf8(&caps.body).map_err(|e| anyhow!("capabilities body not utf-8: {e}"))?;
    if !body.contains("<Capabilities") {
        return Err(anyhow!(
            "capabilities body missing <Capabilities> root: {}",
            snippet(body)
        ));
    }
    for layer in LAYERS {
        let needle = format!(">{layer}<");
        if !body.contains(&needle) {
            return Err(anyhow!("capabilities missing layer identifier {layer}"));
        }
    }
    if !body.contains(TMS_ID) {
        return Err(anyhow!("capabilities missing tile matrix set {TMS_ID}"));
    }

    // REST GetTile against the single level-0 tile. body must be a valid PNG;
    // pixel content is not asserted here (the rendering scenario owns that).
    let tile = http::get(
        client.clone(),
        ns,
        "mars-e2e-runtime",
        8080,
        &format!("/wmts/land/default/{TMS_ID}/0/0/0.png"),
    )
    .await?;
    if tile.status != 200 {
        return Err(anyhow!(
            "GetTile status {} body={}",
            tile.status,
            snippet_bytes(&tile.body)
        ));
    }
    if tile.body.len() < PNG_MAGIC.len() || tile.body[..PNG_MAGIC.len()] != PNG_MAGIC {
        return Err(anyhow!(
            "GetTile body does not start with PNG magic: {}",
            snippet_bytes(&tile.body)
        ));
    }
    Ok(())
}

fn snippet(s: &str) -> String {
    s.chars().take(200).collect()
}

fn snippet_bytes(b: &[u8]) -> String {
    String::from_utf8_lossy(&b[..b.len().min(200)]).into_owned()
}
