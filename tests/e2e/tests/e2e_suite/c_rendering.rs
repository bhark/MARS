//! rendering correctness. WMS GetMap + WMTS GetTile against fixed parameter
//! sets; diff against checked-in goldens with the existing tolerance budget.
//! regenerate via `MARS_E2E_GOLDEN_REGENERATE=1`.

use anyhow::{Context, Result, anyhow};
use mars_e2e_kind::diff;
use mars_e2e_kind::{http, wait};
use std::path::PathBuf;
use std::time::Duration;

use super::scenario::Scenario;

const GOLDEN_DIR: &str = "goldens";
const MAX_CHANNEL_DELTA: u8 = 8;
const MAX_DIFF_RATIO: f32 = 0.02;

struct RenderCase {
    name: &'static str,
    path_and_query: &'static str,
}

const CASES: &[RenderCase] = &[RenderCase {
    name: "wms_full",
    path_and_query: "/wms?service=WMS&version=1.3.0&request=GetMap&layers=land,water,settlements,roads,buildings,waterways,poi&styles=&crs=EPSG:25832&bbox=850000,6090000,895000,6145000&width=512&height=512&format=image/png",
}];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rendering_goldens() -> Result<()> {
    let scenario = Scenario::up("rendering").await?;
    let client = scenario.client.clone();
    let ns = &scenario.ns.name;

    // ensure runtime is serving before we render.
    wait::until("runtime /readyz returns 200", Duration::from_secs(300), || async {
        let r = http::get(client.clone(), ns, "mars-e2e-runtime", 8080, "/readyz").await?;
        if r.status == 200 { Ok(Some(())) } else { Ok(None) }
    })
    .await?;

    let regenerate = std::env::var_os("MARS_E2E_GOLDEN_REGENERATE").is_some();
    let goldens = goldens_dir()?;
    std::fs::create_dir_all(&goldens).context("ensure goldens dir")?;

    for case in CASES {
        let r = http::get(client.clone(), ns, "mars-e2e-runtime", 8080, case.path_and_query).await?;
        if r.status != 200 {
            return Err(anyhow!(
                "case {}: status {} ({} bytes)",
                case.name,
                r.status,
                r.body.len()
            ));
        }
        let golden_path = goldens.join(format!("{}.png", case.name));
        if regenerate {
            std::fs::write(&golden_path, &r.body).with_context(|| format!("write golden {}", golden_path.display()))?;
            eprintln!("regenerated golden: {}", golden_path.display());
            continue;
        }
        let golden = std::fs::read(&golden_path).with_context(|| {
            format!(
                "read golden {} (run with MARS_E2E_GOLDEN_REGENERATE=1 to bootstrap)",
                golden_path.display()
            )
        })?;
        let report =
            diff::diff_pngs(&r.body, &golden, MAX_CHANNEL_DELTA).map_err(|e| anyhow!("diff {}: {e}", case.name))?;
        if report.diff_ratio() > MAX_DIFF_RATIO {
            // write actual + diff next to golden for inspection on failure.
            let out_dir = goldens.join(format!("_failed/{}", case.name));
            std::fs::create_dir_all(&out_dir).ok();
            std::fs::write(out_dir.join("actual.png"), &r.body).ok();
            return Err(anyhow!(
                "case {} diff exceeds tolerance: {} (max_ratio={MAX_DIFF_RATIO})",
                case.name,
                report,
            ));
        }
        eprintln!("case {}: {}", case.name, report);
    }
    Ok(())
}

fn goldens_dir() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("get cwd")?;
    Ok(cwd.join(GOLDEN_DIR))
}
