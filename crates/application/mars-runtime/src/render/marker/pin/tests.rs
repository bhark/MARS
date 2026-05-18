#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mars_style::{MarkerShape, ResolvedMarker};

use super::super::{bbox_of, path_at};

#[test]
fn marker_pin_tip_is_at_anchor_bulb_above() {
    let pos = (10.0, 100.0);
    let p = path_at(
        &ResolvedMarker {
            shape: MarkerShape::Pin,
            size: 8.0,
            rotation_rad: None,
        },
        pos,
    );
    assert!(p.subpaths[0].closed);
    let (_, miny, _, maxy) = bbox_of(&p);
    // tip at pos.1 = 100; bulb extends upward (smaller y in pixel space).
    assert!((maxy - 100.0).abs() < 0.5, "pin tip not at anchor: maxy={maxy}");
    assert!(miny < 100.0 - 4.0, "pin bulb not above tip: miny={miny}");
}
