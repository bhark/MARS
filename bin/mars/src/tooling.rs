//! Operational tooling subcommands: `validate`, `inspect`, `healthcheck`,
//! `setup`, `teardown`. Each is a one-shot exit-immediately path that does
//! not participate in the service-mode dispatch.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use mars_config::Config;

use crate::composition::load_and_validate;

pub(crate) fn validate(path: &Path) -> Result<()> {
    let mut cfg = mars_config::load(path).with_context(|| format!("load {}", path.display()))?;
    mars_config::validate(&mut cfg, &mars_config::config_dir(path)).context("validate")?;
    println!("ok");
    Ok(())
}

pub(crate) fn inspect(path: &Path) -> Result<()> {
    let bytes = std::fs::read(path).map_err(|e| anyhow!("read {}: {}", path.display(), e))?;
    let reader = mars_artifact::ArtifactReader::open(bytes::Bytes::from(bytes))?;
    let bbox = reader.bbox();
    println!("kind: {:?}", reader.kind());
    println!("bbox: [{}, {}, {}, {}]", bbox.min_x, bbox.min_y, bbox.max_x, bbox.max_y);
    println!("feature_count: {}", reader.feature_count());
    if let Some(sr) = reader.source_ref() {
        println!(
            "source_ref: collection={} band={} cell=({},{})",
            sr.collection, sr.band, sr.cell_x, sr.cell_y
        );
    }
    println!("sections:");
    for kind in [
        mars_artifact::SectionKind::SpatialIndex,
        mars_artifact::SectionKind::GeometryPayload,
        mars_artifact::SectionKind::Attributes,
        mars_artifact::SectionKind::LabelCandidates,
        mars_artifact::SectionKind::ClassAssignment,
        mars_artifact::SectionKind::StyleRefs,
    ] {
        match reader.section(kind) {
            Ok(b) => println!("  - {kind:?}: {} bytes", b.len()),
            Err(mars_artifact::ArtifactError::SectionMissing(_)) => {}
            Err(e) => return Err(e.into()),
        }
    }
    // β.4: surface per-(layer, page) unmatched-slot diagnostic. when both
    // geometry and class-assignment are present, the difference is the
    // unmatched-slot count; β.2 should keep this at zero for single-layer-
    // per-binding pages. when only one is present (page-only or sidecar-
    // only artifact), report what's available so operators can cross-
    // reference manually.
    let geom_slots = match reader.section(mars_artifact::SectionKind::SpatialIndex) {
        Ok(b) => Some(mars_artifact::SpatialIndex::open(b)?.len() as usize),
        Err(mars_artifact::ArtifactError::SectionMissing(_)) => None,
        Err(e) => return Err(e.into()),
    };
    let class_slots = match reader.section(mars_artifact::SectionKind::ClassAssignment) {
        Ok(b) => Some(mars_artifact::decode_class_assignment(&b)?.len()),
        Err(mars_artifact::ArtifactError::SectionMissing(_)) => None,
        Err(e) => return Err(e.into()),
    };
    let label_slots = match reader.section(mars_artifact::SectionKind::LabelCandidates) {
        Ok(b) => Some(mars_artifact::decode_label_candidates(&b)?.len()),
        Err(mars_artifact::ArtifactError::SectionMissing(_)) => None,
        Err(e) => return Err(e.into()),
    };
    if let Some(g) = geom_slots {
        println!("geometry slots: {g}");
    }
    if let Some(c) = class_slots {
        println!("class assignments: {c}");
    }
    if let Some(l) = label_slots {
        println!("label candidates: {l}");
    }
    if let (Some(g), Some(c)) = (geom_slots, class_slots) {
        let unmatched = g.saturating_sub(c);
        println!("unmatched slots: {unmatched} (geom - class)");
    }
    Ok(())
}

pub(crate) fn healthcheck(url: &str) -> Result<()> {
    let resp = reqwest::blocking::get(url).with_context(|| format!("healthcheck: request to {url}"))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(anyhow!("healthcheck: {url} returned {status}"))
    }
}

pub(crate) async fn setup(config: &Path, admin_dsn: &str, runtime_password: String, dry_run: bool) -> Result<()> {
    let cfg = load_and_validate(config)?;
    let plan = build_bootstrap_plan(&cfg, runtime_password)?;
    if dry_run {
        for stmt in mars_source_postgres::bootstrap::render_statements(&plan)? {
            println!("{stmt}");
        }
        println!("{}", mars_source_postgres::bootstrap::render_slot_creation(&plan));
        return Ok(());
    }
    tracing::info!(
        role = %plan.role,
        publication = %plan.publication,
        slot = %plan.slot,
        schemas = ?plan.schemas,
        "applying bootstrap",
    );
    mars_source_postgres::bootstrap::apply(admin_dsn, &plan)
        .await
        .context("bootstrap apply")?;
    Ok(())
}

pub(crate) async fn teardown(
    config: &Path,
    admin_dsn: &str,
    drop_slot: bool,
    drop_publication: bool,
    drop_role: bool,
    dry_run: bool,
) -> Result<()> {
    let cfg = load_and_validate(config)?;
    let pg = unique_bootstrap_postgis(&cfg)?;
    let bs = pg
        .bootstrap
        .as_ref()
        .ok_or_else(|| anyhow!("sources[].bootstrap is not configured"))?;
    let cf = pg
        .change_feed
        .as_ref()
        .ok_or_else(|| anyhow!("sources[].change_feed is not configured"))?;
    let plan = mars_source_postgres::bootstrap::TeardownPlan {
        role: bs.role.clone(),
        publication: cf.publication.clone().unwrap_or_default(),
        slot: cf.slot.clone().unwrap_or_default(),
        drop_slot,
        drop_publication,
        drop_role,
    };
    if dry_run {
        for stmt in mars_source_postgres::bootstrap::render_teardown_statements(&plan)? {
            println!("{stmt}");
        }
        return Ok(());
    }
    tracing::info!(
        role = %plan.role,
        publication = %plan.publication,
        slot = %plan.slot,
        drop_slot = plan.drop_slot,
        drop_publication = plan.drop_publication,
        drop_role = plan.drop_role,
        "applying teardown",
    );
    mars_source_postgres::bootstrap::teardown(admin_dsn, &plan)
        .await
        .context("bootstrap teardown")?;
    Ok(())
}

fn build_bootstrap_plan(
    cfg: &Config,
    runtime_password: String,
) -> Result<mars_source_postgres::bootstrap::BootstrapPlan> {
    let pg = unique_bootstrap_postgis(cfg)?;
    let bs = pg
        .bootstrap
        .as_ref()
        .ok_or_else(|| anyhow!("sources[].bootstrap is not configured"))?;
    let cf = pg
        .change_feed
        .as_ref()
        .ok_or_else(|| anyhow!("sources[].change_feed is not configured"))?;
    let publication = cf
        .publication
        .clone()
        .ok_or_else(|| anyhow!("sources[].change_feed.publication is required for bootstrap"))?;
    let slot = cf
        .slot
        .clone()
        .ok_or_else(|| anyhow!("sources[].change_feed.slot is required for bootstrap"))?;
    Ok(mars_source_postgres::bootstrap::BootstrapPlan {
        role: bs.role.clone(),
        runtime_password,
        publication,
        slot,
        schemas: bs.schemas.clone(),
    })
}

/// Pick the unique postgis source carrying a `bootstrap:` block. `mars setup`
/// and `mars teardown` operate on a single source; if more than one is
/// configured with bootstrap, fail fast so the operator names the target
/// explicitly in a future revision.
fn unique_bootstrap_postgis(cfg: &Config) -> Result<&mars_config::PostgisBackend> {
    let mut pg_bootstraps = cfg
        .sources
        .iter()
        .filter_map(|s| s.postgis())
        .filter(|pg| pg.bootstrap.is_some());
    let first = pg_bootstraps
        .next()
        .ok_or_else(|| anyhow!("no postgis source with sources[].bootstrap configured"))?;
    if pg_bootstraps.next().is_some() {
        return Err(anyhow!(
            "more than one postgis source declares sources[].bootstrap; \
             mars setup / teardown currently target a single source"
        ));
    }
    Ok(first)
}
