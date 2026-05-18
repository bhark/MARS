#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn bands() -> Vec<BandConfig> {
    vec![
        BandConfig {
            name: BandName::new("ultra"),
            max_denom: 4_000,
            origin: (0.0, 0.0),
            cell_size: 1024.0,
        },
        BandConfig {
            name: BandName::new("hi"),
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
