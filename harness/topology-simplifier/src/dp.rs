//! per-arc Douglas-Peucker simplification.
//!
//! kernel logic (perp_distance_sq + recursive farthest-point split) mirrors
//! crates/application/mars-compiler/src/decimate.rs:106-147. it's copied here
//! rather than imported because the harness is workspace-excluded; the kernel
//! is small and stable, the duplication is intentional and called out in the
//! plan.
//!
//! arcs come in canonical-direction vertex-id sequence; DP runs on the
//! materialised Coord sequence. open arcs keep their endpoints fixed. closed
//! island arcs (first == last) fall back to the original vertex list when DP
//! would collapse them below the 4-point ring floor — same conservative rule
//! the compiler's naive simplifier uses.

use mars_artifact::Coord;

use crate::graph::{Arc, Topology};

/// simplified per-arc coord sequences, indexed by ArcId. for closed island
/// arcs the first and last entries are equal.
#[derive(Debug, Clone, Default)]
pub struct SimplifiedArcs {
    pub arcs: Vec<Vec<Coord>>,
}

pub fn simplify_arcs(topo: &Topology, tolerance_m: f64) -> SimplifiedArcs {
    let mut out = Vec::with_capacity(topo.arcs.len());
    for arc in &topo.arcs {
        out.push(simplify_one(topo, arc, tolerance_m));
    }
    SimplifiedArcs { arcs: out }
}

fn simplify_one(topo: &Topology, arc: &Arc, tolerance_m: f64) -> Vec<Coord> {
    let coords: Vec<Coord> = arc
        .canonical
        .iter()
        .map(|&v| topo.vertices.get(v as usize).copied().unwrap_or((0.0, 0.0)))
        .collect();
    if tolerance_m <= 0.0 || !tolerance_m.is_finite() || coords.len() <= 2 {
        return coords;
    }
    let is_island = coords.first() == coords.last() && arc.canonical.first() == arc.canonical.last();
    if is_island {
        // closed: ring with first == last, length >= 4 expected for a real
        // island. DP a closed ring by simplifying the open form (drop the
        // duplicate), endpoints kept. fall back when result is too short.
        if coords.len() < 4 {
            return coords;
        }
        let simplified = dp_simplify(&coords, tolerance_m);
        if simplified.len() < 4 {
            return coords;
        }
        simplified
    } else {
        dp_simplify(&coords, tolerance_m)
    }
}

fn dp_simplify(line: &[Coord], tolerance_m: f64) -> Vec<Coord> {
    let mut keep = vec![false; line.len()];
    keep[0] = true;
    keep[line.len() - 1] = true;
    dp(line, 0, line.len() - 1, tolerance_m, &mut keep);
    line.iter()
        .zip(keep)
        .filter_map(|(c, k)| if k { Some(*c) } else { None })
        .collect()
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
    let t = ((px - ax) * dx + (py - ay) * dy) / len2;
    let t_clamped = t.clamp(0.0, 1.0);
    let cx = ax + t_clamped * dx;
    let cy = ay + t_clamped * dy;
    let ex = px - cx;
    let ey = py - cy;
    ex * ex + ey * ey
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::graph::build_topology;
    use mars_artifact::{FeatureGeom, GeomKind};

    fn poly(id: u64, ring: Vec<Coord>) -> FeatureGeom {
        FeatureGeom {
            user_id: id,
            bbox: [0.0, 0.0, 0.0, 0.0],
            geom: GeomKind::Polygon(vec![ring]),
        }
    }

    #[test]
    fn shared_arc_is_simplified_once() {
        // two squares sharing the seam (10,0)-(10,5)-(10,10).
        // (10,5) is collinear and should drop at modest tolerance.
        let p1 = poly(
            1,
            vec![
                (0.0, 0.0),
                (10.0, 0.0),
                (10.0, 5.0),
                (10.0, 10.0),
                (0.0, 10.0),
                (0.0, 0.0),
            ],
        );
        let p2 = poly(
            2,
            vec![
                (10.0, 0.0),
                (20.0, 0.0),
                (20.0, 10.0),
                (10.0, 10.0),
                (10.0, 5.0),
                (10.0, 0.0),
            ],
        );
        let (topo, stats) = build_topology(&[p1, p2], 1);
        // the seam is shared, so it appears as a single shared arc.
        assert_eq!(stats.shared_arc_count, 1);
        let simplified = simplify_arcs(&topo, 0.5);
        // find the shared arc: it has shared_count >= 2 and exactly 3 verts (10,0),(10,5),(10,10)
        let shared_idx = topo
            .arcs
            .iter()
            .position(|a| a.shared_count >= 2)
            .expect("a shared arc exists");
        let simp = &simplified.arcs[shared_idx];
        // collinear midpoint dropped → 2 verts (the junction endpoints).
        assert_eq!(simp.len(), 2);
        let endpoints: std::collections::HashSet<(i64, i64)> =
            simp.iter().map(|(x, y)| (*x as i64, *y as i64)).collect();
        assert!(endpoints.contains(&(10, 0)));
        assert!(endpoints.contains(&(10, 10)));
    }

    #[test]
    fn island_falls_back_when_dp_collapses() {
        // small triangle island. DP at huge tolerance would collapse to <4
        // verts; expect fallback to original.
        let triangle = vec![(0.0, 0.0), (1.0, 0.0), (0.5, 0.5), (0.0, 0.0)];
        let p = poly(1, triangle.clone());
        let (topo, _) = build_topology(&[p], 1);
        let simp = simplify_arcs(&topo, 100.0);
        assert_eq!(simp.arcs.len(), 1);
        // first == last preserved
        assert_eq!(simp.arcs[0].first(), simp.arcs[0].last());
        assert_eq!(simp.arcs[0].len(), 4);
    }

    #[test]
    fn island_simplifies_collinear_midpoint() {
        // square with collinear midpoint on top edge (5, 10).
        let ring = vec![
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (5.0, 10.0),
            (0.0, 10.0),
            (0.0, 0.0),
        ];
        let p = poly(1, ring);
        let (topo, _) = build_topology(&[p], 1);
        let simp = simplify_arcs(&topo, 0.5);
        assert_eq!(simp.arcs.len(), 1);
        // (5,10) drops; closed ring length goes from 6 to 5.
        assert_eq!(simp.arcs[0].len(), 5);
        assert_eq!(simp.arcs[0].first(), simp.arcs[0].last());
    }

    #[test]
    fn zero_tolerance_is_identity() {
        let p = poly(
            1,
            vec![(0.0, 0.0), (1.0, 0.5), (2.0, 0.0), (2.0, 1.0), (0.0, 1.0), (0.0, 0.0)],
        );
        let (topo, _) = build_topology(&[p], 1);
        let simp = simplify_arcs(&topo, 0.0);
        assert_eq!(simp.arcs[0].len(), 6);
    }
}
