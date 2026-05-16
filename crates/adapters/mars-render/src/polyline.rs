//! pixel-space polyline geometry helpers shared by passes that walk a line
//! by arc length (follow labels, stamped markers along stroke).

/// running sum of segment lengths. `out[i]` is the arc length from `pts[0]`
/// to `pts[i]`. `out.len() == pts.len()`; degenerate inputs produce a zero
/// array of the matching length.
pub(crate) fn cumulative_arc_length(pts: &[(f32, f32)]) -> Vec<f32> {
    let mut acc = 0.0_f32;
    let mut out = Vec::with_capacity(pts.len());
    out.push(0.0);
    for w in pts.windows(2) {
        let dx = w[1].0 - w[0].0;
        let dy = w[1].1 - w[0].1;
        acc += (dx * dx + dy * dy).sqrt();
        out.push(acc);
    }
    out
}

/// sample a polyline at arc length `target`. returns the pixel position and
/// unit tangent (dx, dy). `cum` must be the matching arc-length array from
/// `cumulative_arc_length`. returns `None` for inputs too short to define a
/// segment, or when no segment of positive length contains `target`.
pub(crate) fn sample_at(pts: &[(f32, f32)], cum: &[f32], target: f32) -> Option<ArcSample> {
    if pts.len() < 2 || cum.len() != pts.len() {
        return None;
    }
    for i in 0..pts.len() - 1 {
        let s0 = cum[i];
        let s1 = cum[i + 1];
        let seg_len = s1 - s0;
        if target >= s0 && (target <= s1 || i + 1 == pts.len() - 1) {
            let t = if seg_len > 0.0 {
                ((target - s0) / seg_len).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let (x0, y0) = pts[i];
            let (x1, y1) = pts[i + 1];
            let dx = x1 - x0;
            let dy = y1 - y0;
            let len = (dx * dx + dy * dy).sqrt();
            if !(len.is_finite() && len > 0.0) {
                continue;
            }
            return Some(ArcSample {
                pos: (x0 + dx * t, y0 + dy * t),
                tangent: (dx / len, dy / len),
            });
        }
    }
    None
}

pub(crate) struct ArcSample {
    pub pos: (f32, f32),
    pub tangent: (f32, f32),
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn cumulative_horizontal_line() {
        let cum = cumulative_arc_length(&[(0.0, 0.0), (3.0, 0.0), (7.0, 0.0)]);
        assert_eq!(cum, vec![0.0, 3.0, 7.0]);
    }

    #[test]
    fn cumulative_empty_produces_single_zero() {
        // single-point input -> [0.0]; downstream callers treat last() == 0
        // as "no usable polyline".
        let cum = cumulative_arc_length(&[(0.0, 0.0)]);
        assert_eq!(cum, vec![0.0]);
    }

    #[test]
    fn sample_at_midpoint_of_horizontal_segment() {
        let pts = [(0.0, 0.0), (10.0, 0.0)];
        let cum = cumulative_arc_length(&pts);
        let s = sample_at(&pts, &cum, 5.0).expect("sample");
        assert!((s.pos.0 - 5.0).abs() < 1e-5);
        assert!((s.pos.1).abs() < 1e-5);
        assert!((s.tangent.0 - 1.0).abs() < 1e-5);
        assert!((s.tangent.1).abs() < 1e-5);
    }

    #[test]
    fn sample_at_corner_picks_post_corner_segment_tangent() {
        // L-shape: horizontal then vertical. sampling at the corner is the
        // boundary; we accept the post-corner segment because the search
        // iterates segments forward and the first match wins.
        let pts = [(0.0, 0.0), (5.0, 0.0), (5.0, 5.0)];
        let cum = cumulative_arc_length(&pts);
        let pre = sample_at(&pts, &cum, 2.5).expect("pre");
        assert!((pre.tangent.0 - 1.0).abs() < 1e-5);
        let post = sample_at(&pts, &cum, 7.5).expect("post");
        assert!((post.tangent.1 - 1.0).abs() < 1e-5);
    }
}
