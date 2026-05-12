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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use mars_render_port::Subpath;

    fn build(points: Vec<(f32, f32)>, closed: bool) -> Option<tiny_skia::Path> {
        build_path(&PortPath {
            subpaths: vec![Subpath { points, closed }],
        })
    }

    #[test]
    fn build_path_drops_subpath_with_single_point() {
        assert!(build(vec![(1.0, 2.0)], false).is_none());
    }

    #[test]
    fn is_fillable_rejects_horizontal_line() {
        let p = build(vec![(0.0, 5.0), (10.0, 5.0)], false).expect("path");
        assert!(!is_fillable(&p));
    }

    #[test]
    fn is_fillable_rejects_vertical_line() {
        let p = build(vec![(5.0, 0.0), (5.0, 10.0)], false).expect("path");
        assert!(!is_fillable(&p));
    }

    #[test]
    fn is_fillable_rejects_collapsed_closed_polygon() {
        // closed ring whose vertices all share the same y - typical of a tiny
        // polygon flattened onto a pixel row by world->pixel projection.
        let p = build(vec![(0.0, 7.0), (4.0, 7.0), (8.0, 7.0)], true).expect("path");
        assert!(!is_fillable(&p));
    }

    #[test]
    fn is_fillable_accepts_proper_polygon() {
        let p = build(vec![(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0)], true).expect("path");
        assert!(is_fillable(&p));
    }
}
