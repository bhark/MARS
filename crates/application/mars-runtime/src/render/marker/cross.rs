//! plus-sign marker. arm half-width = size/6, visually balanced.

use mars_render_port::{Path, Subpath};

pub(super) fn path(size: f32, (cx, cy): (f32, f32)) -> Path {
    let r = size * 0.5;
    let aw = size / 6.0;
    Path {
        subpaths: vec![Subpath {
            points: vec![
                (cx - aw, cy - r),
                (cx + aw, cy - r),
                (cx + aw, cy - aw),
                (cx + r, cy - aw),
                (cx + r, cy + aw),
                (cx + aw, cy + aw),
                (cx + aw, cy + r),
                (cx - aw, cy + r),
                (cx - aw, cy + aw),
                (cx - r, cy + aw),
                (cx - r, cy - aw),
                (cx - aw, cy - aw),
            ],
            closed: true,
        }],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use mars_style::{MarkerShape, ResolvedMarker};

    use super::super::{assert_marker_centred, path_at};

    #[test]
    fn marker_cross_has_twelve_vertices() {
        let p = path_at(
            &ResolvedMarker {
                shape: MarkerShape::Cross,
                size: 12.0,
                rotation_rad: None,
            },
            (0.0, 0.0),
        );
        assert_eq!(p.subpaths[0].points.len(), 12);
        assert_marker_centred(&p, (0.0, 0.0), 12.0, 0.001);
    }
}
