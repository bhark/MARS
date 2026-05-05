//! Grid math: cell coordinates, scale-band selection, tile-matrix-set algorithms.
//!
//! Pure functions called from both compiler and runtime. No I/O.

#![forbid(unsafe_code)]

use mars_types::{Bbox, Cell, ScaleBand};

/// Errors that grid math can return when fed bad input.
#[derive(Debug, thiserror::Error)]
pub enum GridError {
    #[error("scale denominator out of range for any configured band: {0}")]
    NoBandForScale(u32),
    #[error("cell size must be positive (got {0})")]
    NonPositiveCellSize(f64),
    #[error("bbox covers too many cells ({requested} > {limit})")]
    TooManyCells { requested: usize, limit: usize },
    #[error("bbox coordinates must be finite")]
    NonFiniteBbox,
    #[error("bbox is inverted: min must be <= max on both axes")]
    InvertedBbox,
    #[error("cell index would overflow i64 representable range")]
    CellCountOverflow,
}

/// Configuration of a single scale band: name, max-denominator threshold,
/// origin and cell size in canonical CRS units.
#[derive(Debug, Clone)]
pub struct BandConfig {
    pub name: ScaleBand,
    pub max_denom: u32,
    pub origin: (f64, f64),
    pub cell_size: f64,
}

/// Pick the scale band whose `max_denom` is the smallest one strictly greater
/// than the requested `denom`. SPEC §7.2 defines bands as half-open intervals
/// `[prev_max, max_denom)`, so `denom == max_denom` falls into the next band.
///
/// Bands need not be pre-sorted; the function performs the smallest-strictly-
/// greater scan in O(n). Caller convenience over micro-optimisation.
pub fn pick_band(denom: u32, bands: &[BandConfig]) -> Result<&BandConfig, GridError> {
    bands
        .iter()
        .filter(|b| denom < b.max_denom)
        .min_by_key(|b| b.max_denom)
        .ok_or(GridError::NoBandForScale(denom))
}

/// Enumerate every cell in `band` whose footprint intersects `bbox`.
///
/// `max_cells` bounds the output to prevent unbounded allocation from
/// pathological or malicious requests. the caller decides the limit.
///
/// canonical cell-coverage helper: both the snapshot path (planner enumerates
/// cells over a layer extent) and the change-feed path (per-row geometry bbox
/// -> affected cells) call this. cells are produced in row-major order
/// (`y` outer, `x` inner) for deterministic output. a degenerate (zero-area)
/// bbox returns the single cell that contains the point; negative cell
/// coordinates are supported.
pub fn cells_in_bbox(bbox: Bbox, band: &BandConfig, max_cells: usize) -> Result<Vec<Cell>, GridError> {
    if band.cell_size <= 0.0 {
        return Err(GridError::NonPositiveCellSize(band.cell_size));
    }
    if !(bbox.min_x.is_finite() && bbox.max_x.is_finite() && bbox.min_y.is_finite() && bbox.max_y.is_finite()) {
        return Err(GridError::NonFiniteBbox);
    }
    if bbox.min_x > bbox.max_x || bbox.min_y > bbox.max_y {
        return Err(GridError::InvertedBbox);
    }
    let (ox, oy) = band.origin;
    let cs = band.cell_size;

    // floor + range-check before the cast: silent saturation on out-of-range
    // f64-to-i64 would otherwise produce a huge `nx*ny` lie below.
    let x0 = floor_to_i64((bbox.min_x - ox) / cs)?;
    let x1 = floor_to_i64((bbox.max_x - ox) / cs)?;
    let y0 = floor_to_i64((bbox.min_y - oy) / cs)?;
    let y1 = floor_to_i64((bbox.max_y - oy) / cs)?;

    // bbox validation above guarantees x1>=x0, y1>=y0 (origin/cs are finite by config).
    let nx = usize::try_from(x1 - x0 + 1).map_err(|_| GridError::CellCountOverflow)?;
    let ny = usize::try_from(y1 - y0 + 1).map_err(|_| GridError::CellCountOverflow)?;
    let count = nx.checked_mul(ny).ok_or(GridError::CellCountOverflow)?;
    if count > max_cells {
        return Err(GridError::TooManyCells {
            requested: count,
            limit: max_cells,
        });
    }

    let mut out = Vec::with_capacity(count);
    for y in y0..=y1 {
        for x in x0..=x1 {
            out.push(Cell {
                band: band.name.clone(),
                x,
                y,
            });
        }
    }
    Ok(out)
}

#[inline]
fn floor_to_i64(v: f64) -> Result<i64, GridError> {
    if !v.is_finite() {
        return Err(GridError::NonFiniteBbox);
    }
    let f = v.floor();
    // i64 covers ±2^63; f64 representable integers go up to 2^53 exactly,
    // beyond that the floor is meaningless for cell indexing anyway.
    if f < i64::MIN as f64 || f > i64::MAX as f64 {
        return Err(GridError::CellCountOverflow);
    }
    Ok(f as i64)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn bands() -> Vec<BandConfig> {
        vec![
            BandConfig {
                name: ScaleBand::new("ultra"),
                max_denom: 4_000,
                origin: (0.0, 0.0),
                cell_size: 1024.0,
            },
            BandConfig {
                name: ScaleBand::new("hi"),
                max_denom: 25_000,
                origin: (0.0, 0.0),
                cell_size: 4096.0,
            },
        ]
    }

    #[test]
    fn pick_band_selects_smallest_strictly_greater() {
        let bs = bands();
        assert_eq!(pick_band(1_000, &bs).unwrap().name.as_str(), "ultra");
        assert_eq!(pick_band(10_000, &bs).unwrap().name.as_str(), "hi");
    }

    #[test]
    fn pick_band_off_top_errors() {
        let bs = bands();
        assert!(matches!(pick_band(1_000_000, &bs), Err(GridError::NoBandForScale(_))));
    }

    #[test]
    fn cells_in_bbox_inclusive() {
        let bs = bands();
        let b = Bbox::new(0.0, 0.0, 4096.0, 4096.0);
        let cells = cells_in_bbox(b, &bs[1], 1_000_000).unwrap();
        assert_eq!(cells.len(), 4); // (0,0)(1,0)(0,1)(1,1)
    }

    #[test]
    fn cells_in_bbox_crossing_one_boundary() {
        let bs = bands();
        // hi band cell_size=4096; bbox straddles the x=4096 line in cell row 0
        let b = Bbox::new(4000.0, 100.0, 4200.0, 200.0);
        let cells = cells_in_bbox(b, &bs[1], 1_000).unwrap();
        assert_eq!(cells.len(), 2);
        assert_eq!((cells[0].x, cells[0].y), (0, 0));
        assert_eq!((cells[1].x, cells[1].y), (1, 0));
    }

    #[test]
    fn cells_in_bbox_spanning_many_cells() {
        let bs = bands();
        // 3 wide x 4 tall = 12 cells
        let b = Bbox::new(100.0, 100.0, 4096.0 * 3.0 - 1.0, 4096.0 * 4.0 - 1.0);
        let cells = cells_in_bbox(b, &bs[1], 1_000).unwrap();
        assert_eq!(cells.len(), 12);
        // row-major: (0,0),(1,0),(2,0),(0,1)...
        assert_eq!((cells[0].x, cells[0].y), (0, 0));
        assert_eq!((cells[3].x, cells[3].y), (0, 1));
        assert_eq!((cells[11].x, cells[11].y), (2, 3));
    }

    #[test]
    fn cells_in_bbox_zero_area_returns_containing_cell() {
        let bs = bands();
        // point bbox: a degenerate rectangle covers the cell containing it
        let b = Bbox::new(5000.0, 5000.0, 5000.0, 5000.0);
        let cells = cells_in_bbox(b, &bs[1], 10).unwrap();
        assert_eq!(cells.len(), 1);
        assert_eq!((cells[0].x, cells[0].y), (1, 1));
    }

    #[test]
    fn cells_in_bbox_negative_coords() {
        let bs = bands();
        // origin is (0,0); a bbox in the third quadrant yields negative cell coords
        let b = Bbox::new(-5000.0, -5000.0, -100.0, -100.0);
        let cells = cells_in_bbox(b, &bs[1], 10).unwrap();
        assert_eq!(cells.len(), 4);
        assert_eq!((cells[0].x, cells[0].y), (-2, -2));
        assert_eq!((cells[3].x, cells[3].y), (-1, -1));
    }

    #[test]
    fn cells_in_bbox_rejects_overflow() {
        let bs = bands();
        // a moderately-sized bbox above the cap returns TooManyCells
        let b = Bbox::new(0.0, 0.0, 4096.0 * 2_000.0, 4096.0 * 2_000.0);
        let err = cells_in_bbox(b, &bs[1], 1_000_000).unwrap_err();
        assert!(matches!(err, GridError::TooManyCells { .. }));
        // a truly extreme bbox overflows i64-representable cell space
        let b2 = Bbox::new(0.0, 0.0, 1e30, 1e30);
        let err2 = cells_in_bbox(b2, &bs[1], 1_000_000).unwrap_err();
        assert!(matches!(err2, GridError::CellCountOverflow));
    }

    #[test]
    fn cells_in_bbox_rejects_inverted() {
        let bs = bands();
        let b = Bbox::new(10.0, 0.0, 0.0, 10.0);
        assert!(matches!(cells_in_bbox(b, &bs[1], 100), Err(GridError::InvertedBbox)));
    }

    #[test]
    fn cells_in_bbox_rejects_non_finite() {
        let bs = bands();
        let b = Bbox::new(0.0, 0.0, f64::NAN, 10.0);
        assert!(matches!(cells_in_bbox(b, &bs[1], 100), Err(GridError::NonFiniteBbox)));
        let b = Bbox::new(0.0, 0.0, f64::INFINITY, 10.0);
        assert!(matches!(cells_in_bbox(b, &bs[1], 100), Err(GridError::NonFiniteBbox)));
    }

    #[test]
    fn cells_in_bbox_rejects_extreme_floats() {
        let bs = bands();
        // far beyond i64 representable cell coordinates
        let b = Bbox::new(0.0, 0.0, 1e30, 1e30);
        let err = cells_in_bbox(b, &bs[1], 100).unwrap_err();
        assert!(matches!(err, GridError::CellCountOverflow));
    }

    #[test]
    fn pick_band_boundary_at_max_denom() {
        let bs = bands();
        // max_denom is exclusive: denom == 4_000 must fall into "hi", not "ultra"
        assert_eq!(pick_band(4_000, &bs).unwrap().name.as_str(), "hi");
        // and one less stays in "ultra"
        assert_eq!(pick_band(3_999, &bs).unwrap().name.as_str(), "ultra");
        // top boundary is no band at all
        assert!(matches!(pick_band(25_000, &bs), Err(GridError::NoBandForScale(_))));
    }

    #[test]
    fn pick_band_works_unsorted() {
        let mut bs = bands();
        bs.swap(0, 1);
        assert_eq!(pick_band(1_000, &bs).unwrap().name.as_str(), "ultra");
        assert_eq!(pick_band(10_000, &bs).unwrap().name.as_str(), "hi");
    }
}
