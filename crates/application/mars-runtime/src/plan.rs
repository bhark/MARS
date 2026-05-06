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

/// derive a scale denominator for the request. canonical CRS is metric
/// (units-per-meter = 1) — validated up front in `mars-config::validate`, so
/// here we can apply the simple formula:
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

/// expand a `RenderPlan` into the flat list of cell-tasks to fetch + draw.
///
/// `canonical_bbox` is the request bbox transformed into the canonical CRS;
/// pass `plan.bbox` directly when the request is already canonical. cell
/// selection always operates in canonical-CRS space — the manifest grid is
/// indexed in that frame.
pub(crate) fn resolve(
    plan: &RenderPlan,
    state: &RuntimeState,
    canonical_bbox: mars_types::Bbox,
) -> Result<Vec<LayerCellTask>, RuntimeError> {
    let denom = denom_from_plan(plan.bbox.width(), plan.width);
    let band = pick_band(denom, &state.bands)?;
    let cells = cells_in_bbox(canonical_bbox, band, crate::MAX_CELLS_PER_REQUEST)?;

    let cap = plan.layers.len().checked_mul(cells.len()).unwrap_or(usize::MAX);
    let mut out = Vec::with_capacity(cap);
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn denom_width_zero() {
        assert_eq!(denom_from_plan(1000.0, 0), u32::MAX);
    }

    #[test]
    fn denom_bbox_width_zero() {
        assert_eq!(denom_from_plan(0.0, 1000), u32::MAX);
    }

    #[test]
    fn denom_bbox_width_negative() {
        assert_eq!(denom_from_plan(-10.0, 1000), u32::MAX);
    }

    #[test]
    fn denom_nan() {
        assert_eq!(denom_from_plan(f64::NAN, 1000), u32::MAX);
    }

    #[test]
    fn denom_inf() {
        assert_eq!(denom_from_plan(f64::INFINITY, 1000), u32::MAX);
    }

    #[test]
    fn denom_normal_case() {
        // bbox_width=1000, width=1000 → denom = 1000 / (1000 * 0.00028) = 1 / 0.00028 ≈ 3571
        let d = denom_from_plan(1000.0, 1000);
        assert_eq!(d, 3571);
    }

    #[test]
    fn denom_clamps_to_one() {
        // very small denominator should clamp to 1
        let d = denom_from_plan(0.0001, 1000);
        assert_eq!(d, 1);
    }
}
