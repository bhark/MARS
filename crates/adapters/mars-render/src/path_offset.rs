//! parallel-offset for polylines (line-decoration cartography).
//!
//! produces a new `PortPath` whose subpaths are translated perpendicular to
//! their direction of travel by `d` (positive = right of travel in y-down
//! pixel space). closed rings are passed through unchanged; the simple
//! miter-bisector offset would otherwise invert holes.

use mars_render_port::{Path as PortPath, Subpath as PortSubpath};

/// build a parallel polyline offset by `d` (perpendicular pixels, positive =
/// right of direction of travel). closed subpaths skip with a warn since
/// the simple miter-bisector offset would invert holes. all subpaths share
/// the same offset distance.
pub(crate) fn offset_polyline(path: &PortPath, d: f32) -> Option<PortPath> {
    let mut out = Vec::with_capacity(path.subpaths.len());
    let mut produced = false;
    for sub in &path.subpaths {
        if sub.points.len() < 2 {
            continue;
        }
        if sub.closed {
            tracing::warn!("stroke_offset_px ignored on closed subpath");
            out.push(sub.clone());
            continue;
        }
        if let Some(offset_sub) = offset_open_subpath(sub, d) {
            out.push(offset_sub);
            produced = true;
        } else {
            out.push(sub.clone());
        }
    }
    if !produced {
        None
    } else {
        Some(PortPath { subpaths: out })
    }
}

fn offset_open_subpath(sub: &PortSubpath, d: f32) -> Option<PortSubpath> {
    let n = sub.points.len();
    if n < 2 {
        return None;
    }
    // per-segment unit tangent and right-hand normal (y-down pixel space).
    let mut segs: Vec<((f32, f32), (f32, f32))> = Vec::with_capacity(n - 1);
    for i in 0..n - 1 {
        let (x0, y0) = sub.points[i];
        let (x1, y1) = sub.points[i + 1];
        let dx = x1 - x0;
        let dy = y1 - y0;
        let len = (dx * dx + dy * dy).sqrt();
        if !(len.is_finite() && len > f32::EPSILON) {
            continue;
        }
        let tx = dx / len;
        let ty = dy / len;
        // right-hand normal in y-down pixel space: rotate tangent +90deg
        // visually-clockwise. tangent (1,0) -> normal (0,+1), i.e. +y
        // (visually down), which is "right of direction of travel" on screen.
        segs.push(((tx, ty), (-ty, tx)));
    }
    if segs.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(n);
    for (j, &(x, y)) in sub.points.iter().enumerate() {
        // pick the surrounding normals; clamp to the closest available
        // segment at the endpoints.
        let n_in = if j == 0 { None } else { segs.get(j - 1).map(|s| s.1) };
        let n_out = segs.get(j).map(|s| s.1);
        let bisector = match (n_in, n_out) {
            (Some(a), Some(b)) => miter_offset(a, b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => continue,
        };
        out.push((x + d * bisector.0, y + d * bisector.1));
    }
    if out.len() < 2 {
        return None;
    }
    Some(PortSubpath {
        points: out,
        closed: false,
    })
}

/// miter offset for adjacent unit right-hand normals. cap the miter factor
/// to absorb tight angles; the v1 contract accepts self-intersection at
/// hairpins.
fn miter_offset(a: (f32, f32), b: (f32, f32)) -> (f32, f32) {
    let dot = a.0 * b.0 + a.1 * b.1;
    let denom = 1.0 + dot;
    if denom.abs() < 1e-3 {
        // near-anti-parallel: fall back to the average normal.
        return ((a.0 + b.0) * 0.5, (a.1 + b.1) * 0.5);
    }
    let f = 1.0 / denom;
    let f = f.clamp(-10.0, 10.0);
    (f * (a.0 + b.0), f * (a.1 + b.1))
}
