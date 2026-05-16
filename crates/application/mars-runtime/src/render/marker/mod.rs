//! point marker tessellation. dispatch hub on `MarkerShape`.
//!
//! adding a marker variant: one new file under `marker/`, one mod line
//! here, one match arm. dispatch is exhaustive (no `_` arm) so a new
//! variant breaks the build here, per `docs/EXTENDING.md` principle 2.
//! shapes are drawn outline-and-fill-friendly (closed subpaths) so the
//! renderer's `draw_path` fills and strokes per the enclosing `Style`.

mod circle;
mod cross;
mod glyph;
mod pin;
mod square;
mod triangle;
mod vector_shape;
mod x;

use mars_render_port::Path;
use mars_style::{MarkerShape, ResolvedMarker};

pub(super) fn path_at(m: &ResolvedMarker, pos: (f32, f32)) -> Path {
    let size = m.size;
    let mut path = match &m.shape {
        MarkerShape::Circle => circle::path(size, pos),
        MarkerShape::Square => square::path(size, pos),
        MarkerShape::Triangle => triangle::path(size, pos),
        MarkerShape::Cross => cross::path(size, pos),
        MarkerShape::X => x::path(size, pos),
        MarkerShape::Pin => pin::path(size, pos),
        MarkerShape::VectorShape { points, anchor, filled } => vector_shape::path(points, *anchor, *filled, size, pos),
        MarkerShape::Glyph { .. } => glyph::path(pos),
    };
    if let Some(theta) = m.rotation_rad
        && theta.abs() > f32::EPSILON
    {
        rotate_subpaths(&mut path, pos, theta);
    }
    path
}

// rotate every vertex around `pivot` by `theta` radians counter-clockwise.
// `Path` is the renderer-port wire type; tiny-skia interprets canvas y as
// positive-down, so a positive theta rotates clockwise on screen - matching
// mapserver's ANGLE semantics.
fn rotate_subpaths(path: &mut Path, pivot: (f32, f32), theta: f32) {
    let (s, c) = theta.sin_cos();
    for sp in &mut path.subpaths {
        for p in &mut sp.points {
            let dx = p.0 - pivot.0;
            let dy = p.1 - pivot.1;
            p.0 = pivot.0 + c * dx - s * dy;
            p.1 = pivot.1 + s * dx + c * dy;
        }
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
