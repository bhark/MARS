//! circle marker - N-segment polygon approximation.

use mars_render_port::{Path, Subpath};

pub(super) fn path(size: f32, (cx, cy): (f32, f32)) -> Path {
    // 24 keeps the outline smooth up to ~32 px without bloating the path.
    const N: usize = 24;
    let r = size * 0.5;
    let pts: Vec<(f32, f32)> = (0..N)
        .map(|i| {
            let theta = (i as f32) * std::f32::consts::TAU / N as f32;
            (cx + r * theta.cos(), cy + r * theta.sin())
        })
        .collect();
    Path {
        subpaths: vec![Subpath {
            points: pts,
            closed: true,
        }],
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use mars_style::{MarkerShape, MarkerSymbol};

    use super::super::{assert_marker_centred, path_at};

    #[test]
    fn marker_circle_is_closed_and_centred() {
        let p = path_at(
            &MarkerSymbol {
                shape: MarkerShape::Circle,
                size: 10.0,
            },
            (50.0, 50.0),
        );
        assert_marker_centred(&p, (50.0, 50.0), 10.0, 0.5);
    }
}
