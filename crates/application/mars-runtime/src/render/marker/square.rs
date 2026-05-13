//! axis-aligned square marker.

use mars_render_port::{Path, Subpath};

pub(super) fn path(size: f32, (cx, cy): (f32, f32)) -> Path {
    let r = size * 0.5;
    Path {
        subpaths: vec![Subpath {
            points: vec![(cx - r, cy - r), (cx + r, cy - r), (cx + r, cy + r), (cx - r, cy + r)],
            closed: true,
        }],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use mars_style::MarkerSymbol;

    use super::super::{assert_marker_centred, path_at};

    #[test]
    fn marker_square_has_four_vertices_and_is_centred() {
        let p = path_at(&MarkerSymbol::Square { size: 8.0 }, (32.0, 16.0));
        assert_marker_centred(&p, (32.0, 16.0), 8.0, 0.001);
        assert_eq!(p.subpaths[0].points.len(), 4);
    }
}
