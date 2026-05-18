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
mod tests;
