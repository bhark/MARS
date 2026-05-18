//! per-level decimation filters for the snapshot / rebuild paths.
//!
//! Decimation: each level emits a render set (geometry pruned by
//! `geometry_min_size_m` and simplified to `vertex_tolerance_m`) and a label
//! set (kept above `label_min_priority`, with survival across levels driven
//! by the layer's `LabelSurvival` policy).
//!
//! everything here is pure: no I/O, no allocation beyond the new geometry.
//! the snapshot/rebuild pipelines call these for every (binding, level).

use mars_artifact::{Coord, FeatureGeom, GeomKind};
use mars_config::SimplifierKind;

/// keep features whose bbox diagonal in canonical-CRS units is at least
/// `min_size_m`. `min_size_m == 0.0` keeps every feature. used to skip the
/// long tail at low zoom levels where sub-pixel features add cost without
/// changing what the renderer draws.
#[must_use]
pub fn passes_min_size(feature: &FeatureGeom, min_size_m: f64) -> bool {
    passes_min_size_bbox(feature.bbox, min_size_m)
}

/// bbox-only variant of [`passes_min_size`]. shares the diagonal-length
/// formulation so pass-1 page planning (which only sees row bboxes) and
/// pass-2 page rendering (which sees decoded geometries) prune identically.
#[must_use]
pub fn passes_min_size_bbox(bbox: [f32; 4], min_size_m: f64) -> bool {
    if min_size_m <= 0.0 {
        return true;
    }
    let dx = f64::from(bbox[2]) - f64::from(bbox[0]);
    let dy = f64::from(bbox[3]) - f64::from(bbox[1]);
    (dx * dx + dy * dy).sqrt() >= min_size_m
}

/// keep label candidates whose `priority` is at or above `min_priority`.
/// matches decimation `label_min_priority` semantics.
#[must_use]
pub fn passes_label_priority(priority: u16, min_priority: u32) -> bool {
    u32::from(priority) >= min_priority
}

/// dispatch entry. selects the simplifier strategy declared on the binding.
#[must_use]
pub fn simplify(geom: &GeomKind, tolerance_m: f64, kind: SimplifierKind) -> GeomKind {
    match kind {
        SimplifierKind::Naive => simplify_naive(geom, tolerance_m),
    }
}

/// douglas-peucker simplification. `tolerance_m == 0.0` returns the geometry
/// unchanged. polygons run per-ring, lines per linestring, multi-* per part.
/// points and multipoints are returned unchanged.
///
/// rings that collapse below four points (the closing duplicate of three
/// distinct vertices) round-trip through the original ring; this is a
/// conservative choice that preserves the polygon at low tolerance and
/// avoids emitting topologically-degenerate output. callers that want
/// coarse pruning rely on `passes_min_size` to drop tiny features first.
#[must_use]
pub fn simplify_naive(geom: &GeomKind, tolerance_m: f64) -> GeomKind {
    if tolerance_m <= 0.0 || !tolerance_m.is_finite() {
        return geom.clone();
    }
    match geom {
        GeomKind::Point(_) | GeomKind::MultiPoint(_) => geom.clone(),
        GeomKind::LineString(line) => GeomKind::LineString(simplify_line(line, tolerance_m)),
        GeomKind::MultiLineString(parts) => {
            GeomKind::MultiLineString(parts.iter().map(|p| simplify_line(p, tolerance_m)).collect())
        }
        GeomKind::Polygon(rings) => GeomKind::Polygon(rings.iter().map(|r| simplify_ring(r, tolerance_m)).collect()),
        GeomKind::MultiPolygon(polys) => GeomKind::MultiPolygon(
            polys
                .iter()
                .map(|rings| rings.iter().map(|r| simplify_ring(r, tolerance_m)).collect())
                .collect(),
        ),
    }
}

fn simplify_line(line: &[Coord], tolerance_m: f64) -> Vec<Coord> {
    if line.len() <= 2 {
        return line.to_vec();
    }
    let mut keep = vec![false; line.len()];
    keep[0] = true;
    keep[line.len() - 1] = true;
    dp(line, 0, line.len() - 1, tolerance_m, &mut keep);
    line.iter()
        .zip(keep)
        .filter_map(|(c, k)| if k { Some(*c) } else { None })
        .collect()
}

fn simplify_ring(ring: &[Coord], tolerance_m: f64) -> Vec<Coord> {
    // a ring with fewer than 4 points (3 distinct + closure) is already
    // degenerate; leave it for downstream validation rather than mutate.
    if ring.len() < 4 {
        return ring.to_vec();
    }
    let simplified = simplify_line(ring, tolerance_m);
    if simplified.len() < 4 {
        // would emit a non-polygon ring: keep original.
        return ring.to_vec();
    }
    simplified
}

fn dp(line: &[Coord], lo: usize, hi: usize, tol: f64, keep: &mut [bool]) {
    if hi <= lo + 1 {
        return;
    }
    let (mut farthest, mut max_d2) = (lo, 0.0f64);
    let tol2 = tol * tol;
    for i in (lo + 1)..hi {
        let d2 = perp_distance_sq(line[i], line[lo], line[hi]);
        if d2 > max_d2 {
            farthest = i;
            max_d2 = d2;
        }
    }
    if max_d2 > tol2 {
        keep[farthest] = true;
        dp(line, lo, farthest, tol, keep);
        dp(line, farthest, hi, tol, keep);
    }
}

#[inline]
fn perp_distance_sq(p: Coord, a: Coord, b: Coord) -> f64 {
    let (px, py) = p;
    let (ax, ay) = a;
    let (bx, by) = b;
    let dx = bx - ax;
    let dy = by - ay;
    let len2 = dx * dx + dy * dy;
    if len2 == 0.0 {
        let ex = px - ax;
        let ey = py - ay;
        return ex * ex + ey * ey;
    }
    // project p onto the line (a..b), clamp to segment, distance squared.
    let t = ((px - ax) * dx + (py - ay) * dy) / len2;
    let t_clamped = t.clamp(0.0, 1.0);
    let cx = ax + t_clamped * dx;
    let cy = ay + t_clamped * dy;
    let ex = px - cx;
    let ey = py - cy;
    ex * ex + ey * ey
}

#[cfg(test)]
mod tests;
