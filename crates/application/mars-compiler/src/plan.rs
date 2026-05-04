//! Translate a typed `Config` into a flat list of per-binding `BuildTask`s.
//!
//! one task = one (layer, band, binding) triple plus the cell list and the
//! pre-parsed class table. the snapshot driver consumes these directly.

use std::collections::BTreeMap;

use mars_config::{Band, ClassStyle, Config, Layer, ScaleWindow, SourceBinding as CfgBinding};
use mars_expr::ExprError;
use mars_grid::{BandConfig, GridError, cells_in_bbox};
use mars_source::{SourceBinding, SourceCollectionId, SourceError};
use mars_types::{Bbox, Cell, CrsCode, LayerId, ScaleBand};

use crate::class::CompiledClass;

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error(transparent)]
    Grid(#[from] GridError),
    #[error(transparent)]
    Expr(#[from] ExprError),
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error("config: {0}")]
    Config(#[from] mars_config::ConfigError),
}

/// One unit of compiler work for the snapshot driver.
#[derive(Debug, Clone)]
pub struct BuildTask {
    pub layer: LayerId,
    pub band: ScaleBand,
    pub binding: SourceBinding,
    pub cells: Vec<Cell>,
    pub cell_size: f64,
    pub origin: (f64, f64),
    pub classes: Vec<CompiledClass>,
}

/// build the full plan from the config.
pub fn build_plan(cfg: &Config) -> Result<Vec<BuildTask>, PlanError> {
    let cell_sizes = cfg.cells.size_per_band_m()?;
    let band_index: BTreeMap<&str, &Band> = cfg.scales.bands.iter().map(|b| (b.name.as_str(), b)).collect();
    let origin = (cfg.cells.origin[0], cfg.cells.origin[1]);
    let extent = cfg.cells.extent.unwrap_or_else(|| {
        // single-cell fallback at the origin for phase-0 in-memory tests
        Bbox::new(origin.0, origin.1, origin.0 + 1.0, origin.1 + 1.0)
    });
    let crs = cfg.source.native_crs.clone();

    let mut tasks = Vec::new();
    for layer in &cfg.layers {
        let classes = compile_classes(layer)?;
        for binding in &layer.sources {
            for (band_name, &cell_size) in &cell_sizes {
                let band = band_index
                    .get(band_name.as_str())
                    .ok_or_else(|| PlanError::Invalid(format!("unknown band {band_name}")))?;
                if !window_intersects(&layer.scale, band) || !window_intersects(&binding.scale, band) {
                    continue;
                }
                let band_cfg = BandConfig {
                    name: ScaleBand::new(band_name.as_str()),
                    max_denom: u32::try_from(band.max_denom).unwrap_or(u32::MAX),
                    origin,
                    cell_size,
                };
                let extent_for_layer = layer.bbox.unwrap_or(extent);
                let cells = cells_in_bbox(extent_for_layer, &band_cfg)?;
                if cells.is_empty() {
                    continue;
                }
                let port_binding = lower_binding(binding, &crs)?;
                tasks.push(BuildTask {
                    layer: layer.name.clone(),
                    band: ScaleBand::new(band_name.as_str()),
                    binding: port_binding,
                    cells,
                    cell_size,
                    origin,
                    classes: classes.clone(),
                });
            }
        }
    }
    Ok(tasks)
}

/// canonical-crs bbox of a single cell.
#[must_use]
pub fn cell_bbox(origin: (f64, f64), cell_size: f64, cell: &Cell) -> Bbox {
    let (ox, oy) = origin;
    let min_x = ox + cell.x as f64 * cell_size;
    let min_y = oy + cell.y as f64 * cell_size;
    Bbox::new(min_x, min_y, min_x + cell_size, min_y + cell_size)
}

fn window_intersects(window: &Option<ScaleWindow>, band: &Band) -> bool {
    // a band spans [prev_max .. band.max_denom). without prev_max here we
    // accept anything overlapping (0, band.max_denom). good enough for phase-0
    // since the test config has one band.
    let Some(w) = window else { return true };
    let band_max = band.max_denom;
    if let Some(min) = w.min
        && min >= band_max
    {
        return false;
    }
    if let Some(max) = w.max
        && max == 0
    {
        return false;
    }
    true
}

fn compile_classes(layer: &Layer) -> Result<Vec<CompiledClass>, PlanError> {
    let mut out = Vec::with_capacity(layer.classes.len());
    for (i, c) in layer.classes.iter().enumerate() {
        let when = match &c.when {
            Some(s) => Some(mars_expr::parse(s)?),
            None => None,
        };
        let style_id = match &c.style {
            ClassStyle::Ref { ref_ } => ref_.clone(),
            ClassStyle::Inline(_) => format!("{}::{}", layer.name, c.name),
        };
        out.push(CompiledClass {
            name: c.name.clone(),
            when,
            style_id,
            class_index: u16::try_from(i)
                .map_err(|_| PlanError::Invalid(format!("layer {} has too many classes", layer.name)))?,
        });
    }
    Ok(out)
}

fn lower_binding(b: &CfgBinding, crs: &CrsCode) -> Result<SourceBinding, PlanError> {
    // `from` is `schema.table`. tolerate single-segment names by routing to
    // the public schema (matches postgres adapter convention).
    let (schema, table) = match b.from.split_once('.') {
        Some((s, t)) => (s.to_string(), t.to_string()),
        None => ("public".to_string(), b.from.clone()),
    };
    let id_column = b.id_column.clone().unwrap_or_else(|| "ogc_fid".to_string());
    let collection = SourceCollectionId::new(b.from.clone());
    Ok(SourceBinding::new(
        collection,
        schema,
        table,
        b.geometry_column.clone(),
        id_column,
        b.attributes.clone(),
        crs.clone(),
    )?)
}
