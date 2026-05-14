//! image-pattern fill scenario. proves the `FillPaint::Image` seam wires
//! end-to-end: operator mounts the bitmap configmap into the compiler pod,
//! the compiler bundles it into the manifest's `image_artifact`, the runtime
//! decodes + binds it into the image registry, and the renderer dispatches
//! a tiled tiny-skia pattern. golden lives at `goldens/wms_image_pattern.png`;
//! regenerate with `MARS_E2E_GOLDEN_REGENERATE=1`.

use anyhow::{Context, Result, anyhow};
use mars_e2e_kind::diff;
use mars_e2e_kind::{http, wait};
use std::path::PathBuf;
use std::time::Duration;

use super::scenario::Scenario;

const GOLDEN_NAME: &str = "wms_image_pattern";
const GOLDEN_DIR: &str = "goldens";
const MAX_CHANNEL_DELTA: u8 = 8;
const MAX_DIFF_RATIO: f32 = 0.02;

// bbox fits exactly the synthetic pattern_zone polygon
// (tests/e2e/sql/synthetic-pattern.sql). at 256x256 the 16x16 stripe tile
// repeats 16 times across the frame, making the pattern unmistakable.
const PATH_AND_QUERY: &str = "/wms?service=WMS&version=1.3.0&request=GetMap\
&layers=pattern_zone&styles=&crs=EPSG:25832\
&bbox=860000,6105000,880000,6125000&width=256&height=256&format=image/png";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn image_pattern_golden() -> Result<()> {
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
        return Err(anyhow!(
            "image_pattern: status {} ({} bytes)",
            r.status,
            r.body.len()
        ));
    }

    let goldens = goldens_dir()?;
    std::fs::create_dir_all(&goldens).context("ensure goldens dir")?;
    let golden_path = goldens.join(format!("{GOLDEN_NAME}.png"));

    if std::env::var_os("MARS_E2E_GOLDEN_REGENERATE").is_some() {
        std::fs::write(&golden_path, &r.body).with_context(|| format!("write golden {}", golden_path.display()))?;
        eprintln!("regenerated golden: {}", golden_path.display());
        return Ok(());
    }

    let golden = std::fs::read(&golden_path).with_context(|| {
        format!(
            "read golden {} (run with MARS_E2E_GOLDEN_REGENERATE=1 to bootstrap)",
            golden_path.display()
        )
    })?;
    let report =
        diff::diff_pngs(&r.body, &golden, MAX_CHANNEL_DELTA).map_err(|e| anyhow!("diff {GOLDEN_NAME}: {e}"))?;
    if report.diff_ratio() > MAX_DIFF_RATIO {
        let out_dir = goldens.join(format!("_failed/{GOLDEN_NAME}"));
        std::fs::create_dir_all(&out_dir).ok();
        std::fs::write(out_dir.join("actual.png"), &r.body).ok();
        return Err(anyhow!(
            "image_pattern diff exceeds tolerance: {report} (max_ratio={MAX_DIFF_RATIO})"
        ));
    }
    eprintln!("image_pattern: {report}");
    Ok(())
}

fn goldens_dir() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("get cwd")?;
    Ok(cwd.join(GOLDEN_DIR))
}
