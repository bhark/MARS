//! point marker tessellation. dispatch hub on `MarkerSymbol` variant.
//!
//! adding a marker variant: one new file under `marker/`, one mod line
//! here, one match arm. `MarkerSymbol` is `#[non_exhaustive]` for serde
//! evolution, so the dispatch keeps a debug-asserting catch-all with a
//! degenerate single-point fallback for release builds. shapes are drawn
//! outline-and-fill-friendly (closed subpaths) so the renderer's
//! `draw_path` fills and strokes per the enclosing `Style`.

mod circle;
mod cross;
mod glyph;
mod pin;
mod square;
mod triangle;
mod vector_shape;
mod x;

use mars_render_port::{Path, Subpath};
use mars_style::MarkerSymbol;

pub(super) fn path_at(m: &MarkerSymbol, pos: (f32, f32)) -> Path {
    match m {
        MarkerSymbol::Circle { size } => circle::path(*size, pos),
        MarkerSymbol::Square { size } => square::path(*size, pos),
        MarkerSymbol::Triangle { size } => triangle::path(*size, pos),
        MarkerSymbol::Cross { size } => cross::path(*size, pos),
        MarkerSymbol::X { size } => x::path(*size, pos),
        MarkerSymbol::Pin { size } => pin::path(*size, pos),
        MarkerSymbol::VectorShape {
            points,
            anchor,
            filled,
            size,
        } => vector_shape::path(points, *anchor, *filled, *size, pos),
        MarkerSymbol::Glyph { .. } => glyph::path(pos),
        // future variants land additively; fail loud in dev/CI so a new
        // variant cannot ship as an invisible marker, but keep a degenerate
        // single-point fallback so release builds still stroke at the
        // anchor.
        other => {
            debug_assert!(false, "unhandled MarkerSymbol variant: {other:?}");
            degenerate(pos)
        }
    }
}

fn degenerate((cx, cy): (f32, f32)) -> Path {
    Path {
        subpaths: vec![Subpath {
            points: vec![(cx, cy)],
            closed: false,
        }],
    }
}

#[cfg(test)]
fn bbox_of(path: &Path) -> (f32, f32, f32, f32) {
    let mut minx = f32::INFINITY;
    let mut miny = f32::INFINITY;
    let mut maxx = f32::NEG_INFINITY;
    let mut maxy = f32::NEG_INFINITY;
    for sp in &path.subpaths {
        for &(x, y) in &sp.points {
            if x < minx {
                minx = x;
            }
            if y < miny {
                miny = y;
            }
            if x > maxx {
                maxx = x;
            }
            if y > maxy {
                maxy = y;
            }
        }
    }
    (minx, miny, maxx, maxy)
}

#[cfg(test)]
fn assert_marker_centred(path: &Path, pos: (f32, f32), expected_extent: f32, tol: f32) {
    assert!(!path.subpaths.is_empty(), "empty path");
    for sp in &path.subpaths {
        assert!(sp.closed, "marker subpath must be closed");
    }
    let (minx, miny, maxx, maxy) = bbox_of(path);
    let cx = (minx + maxx) * 0.5;
    let cy = (miny + maxy) * 0.5;
    let w = maxx - minx;
    let h = maxy - miny;
    assert!((cx - pos.0).abs() < tol, "x centre off: {cx} vs {}", pos.0);
    assert!((cy - pos.1).abs() < tol, "y centre off: {cy} vs {}", pos.1);
    assert!(
        (w - expected_extent).abs() < tol && (h - expected_extent).abs() < tol,
        "extent {w}x{h} != {expected_extent}",
    );
}
