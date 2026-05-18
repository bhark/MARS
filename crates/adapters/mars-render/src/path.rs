//! port-path -> tiny-skia path conversion and fillability gate.

use mars_render_port::Path as PortPath;
use tiny_skia::PathBuilder;

/// build a tiny-skia path from port subpaths. closed subpaths are finished
/// with `close()`, open ones are left open.
/// returns None if no subpath has at least 2 points (tiny-skia rejects empty paths).
pub(crate) fn build_path(path: &PortPath) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    let mut any = false;
    for sub in &path.subpaths {
        if sub.points.len() < 2 {
            continue;
        }
        let (x0, y0) = sub.points[0];
        pb.move_to(x0, y0);
        for &(x, y) in &sub.points[1..] {
            pb.line_to(x, y);
        }
        if sub.closed {
            pb.close();
        }
        any = true;
    }
    if !any {
        return None;
    }
    pb.finish()
}

/// true iff the path's AABB has non-zero extent on both axes. tiny-skia's
/// `fill_path` rejects degenerate-bbox paths (collapsed to a point or a
/// horizontal/vertical line) with a `log::warn`; gating here suppresses that
/// noise for the common case of subpixel polygons after world->pixel
/// projection. threshold mirrors tiny-skia's `SCALAR_NEARLY_ZERO` (1/4096).
pub(crate) fn is_fillable(path: &tiny_skia::Path) -> bool {
    const NEARLY_ZERO: f32 = 1.0 / 4096.0;
    let b = path.bounds();
    b.width() > NEARLY_ZERO && b.height() > NEARLY_ZERO
}

#[cfg(test)]
mod tests;
