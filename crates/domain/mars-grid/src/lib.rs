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
/// than the requested `denom`. Bands must be passed sorted by `max_denom` ascending.
pub fn pick_band(denom: u32, bands: &[BandConfig]) -> Result<&BandConfig, GridError> {
    debug_assert!(
        bands.windows(2).all(|w| w[0].max_denom <= w[1].max_denom),
        "bands must be sorted by max_denom ascending",
    );
    bands
        .iter()
        .find(|b| denom < b.max_denom)
        .ok_or(GridError::NoBandForScale(denom))
}

/// Enumerate every cell in `band` whose footprint intersects `bbox`.
pub fn cells_in_bbox(bbox: Bbox, band: &BandConfig) -> Result<Vec<Cell>, GridError> {
    if band.cell_size <= 0.0 {
        return Err(GridError::NonPositiveCellSize(band.cell_size));
    }
    let (ox, oy) = band.origin;
    let cs = band.cell_size;
    let x0 = ((bbox.min_x - ox) / cs).floor() as i64;
    let x1 = ((bbox.max_x - ox) / cs).floor() as i64;
    let y0 = ((bbox.min_y - oy) / cs).floor() as i64;
    let y1 = ((bbox.max_y - oy) / cs).floor() as i64;

    let mut out = Vec::with_capacity(((x1 - x0 + 1).max(0) * (y1 - y0 + 1).max(0)) as usize);
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
        let cells = cells_in_bbox(b, &bs[1]).unwrap();
        assert_eq!(cells.len(), 4); // (0,0)(1,0)(0,1)(1,1)
    }
}
