//! pure plan resolution: render plan → list of (layer, cell) tasks. no i/o.

use mars_grid::{cells_in_bbox, pick_band};
use mars_types::{Cell, LayerId};

use crate::{RenderPlan, RuntimeError, state::RuntimeState};

/// one cell of work for one layer. ordered: outer = layer (declared order),
/// inner = grid (row-major from `cells_in_bbox`).
#[derive(Debug, Clone)]
pub(crate) struct LayerCellTask {
    pub layer: LayerId,
    pub cell: Cell,
}

/// OGC 1.3.0 standard pixel size used to derive a denominator from a bbox.
const OGC_PIXEL_M: f64 = 0.000_28;

/// derive a scale denominator for the request. phase-0 simplification: the
/// canonical CRS is metric, units-per-meter = 1, so:
///   denom = bbox.width / (width_px * OGC_PIXEL_M)
#[must_use]
pub fn denom_from_plan(bbox_width: f64, width_px: u32) -> u32 {
    if width_px == 0 || bbox_width <= 0.0 {
        return u32::MAX;
    }
    let d = bbox_width / (f64::from(width_px) * OGC_PIXEL_M);
    if !d.is_finite() || d <= 0.0 {
        u32::MAX
    } else {
        d.round().clamp(1.0, f64::from(u32::MAX)) as u32
    }
}

/// hard limit on cells a single request may cover. prevents oom from
/// pathological bbox / tiny cell size combinations.
const MAX_CELLS_PER_REQUEST: usize = 1_000_000;

/// expand a `RenderPlan` into the flat list of cell-tasks to fetch + draw.
pub(crate) fn resolve(plan: &RenderPlan, state: &RuntimeState) -> Result<Vec<LayerCellTask>, RuntimeError> {
    let denom = denom_from_plan(plan.bbox.width(), plan.width);
    let band = pick_band(denom, &state.bands)?;
    let cells = cells_in_bbox(plan.bbox, band, MAX_CELLS_PER_REQUEST)?;

    let mut out = Vec::with_capacity(plan.layers.len() * cells.len());
    for layer in &plan.layers {
        for cell in &cells {
            out.push(LayerCellTask {
                layer: layer.clone(),
                cell: cell.clone(),
            });
        }
    }
    Ok(out)
}
