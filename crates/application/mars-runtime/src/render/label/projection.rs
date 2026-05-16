//! polyline pixel-space projection + arc-length sampling.

use crate::RenderPlan;
use crate::render::project::world_to_pixel;

pub(super) fn project_polyline_to_pixels(
    points: &[(f32, f32)],
    xform: Option<&mars_proj::Transformer>,
    plan: &RenderPlan,
) -> Option<Vec<(f32, f32)>> {
    let mut out = Vec::with_capacity(points.len());
    for &(x, y) in points {
        let (wx, wy) = match xform {
            None => (f64::from(x), f64::from(y)),
            Some(t) => t.transform_point(f64::from(x), f64::from(y)).ok()?,
        };
        out.push(world_to_pixel((wx, wy), plan.bbox, plan.width, plan.height));
    }
    Some(out)
}

pub(super) fn cumulative_arc_length(pts: &[(f32, f32)]) -> Vec<f32> {
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

pub(super) struct ArcSample {
    pub(super) pos: (f32, f32),
    pub(super) tangent: (f32, f32),
}

pub(super) fn sample_at(pts: &[(f32, f32)], cum: &[f32], target: f32) -> Option<ArcSample> {
    if pts.len() < 2 || cum.len() != pts.len() {
        return None;
    }
    // first segment whose end >= target. linear scan; polylines on the hot
    // path are short enough to make a binary search overhead-not-worth-it.
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

pub(super) fn atan2_signed((tx, ty): (f32, f32)) -> f32 {
    ty.atan2(tx)
}

pub(super) fn angle_diff(a: f32, b: f32) -> f32 {
    let mut d = a - b;
    while d > std::f32::consts::PI {
        d -= std::f32::consts::TAU;
    }
    while d < -std::f32::consts::PI {
        d += std::f32::consts::TAU;
    }
    d
}
