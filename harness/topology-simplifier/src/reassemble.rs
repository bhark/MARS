//! polygon reassembly + validity sweep.
//!
//! walks the input geometry tree in lockstep with topo.rings (which were
//! produced in the same input-traversal order), splices the per-arc
//! simplified coord sequences back into rings, and emits FeatureGeoms with
//! the same Polygon / MultiPolygon shape as the input.
//!
//! validity rules mirror decimate.rs: a ring that simplifies below 4 points
//! falls back to the original; counters tally degenerate cases. extra cheap
//! checks not in the compiler version (we are stress-testing topology):
//!   - collapsed_arc_count: arcs reduced to 2 vertices (endpoints only)
//!   - invalid_reassembly_count: a hole's first vertex now sits outside its
//!     containing shell
//!   - self_intersection_count: a ring contains a proper segment crossing
//!     between non-adjacent segments

// `features_in` is reported by the gate write-up landing later; keep it on
// the struct so the report path doesn't have to rederive it.
#![allow(dead_code)]

use mars_artifact::{Coord, FeatureGeom, GeomKind};

use crate::dp::SimplifiedArcs;
use crate::graph::{Direction, RingArcs, Topology};

#[derive(Debug, Default, Clone)]
pub struct ReassemblyStats {
    pub features_in: u64,
    pub features_out: u64,
    pub rings_total: u64,
    pub collapsed_ring_count: u64,
    pub collapsed_arc_count: u64,
    pub invalid_reassembly_count: u64,
    pub self_intersection_count: u64,
}

pub fn reassemble(
    geoms: &[FeatureGeom],
    topo: &Topology,
    simp: &SimplifiedArcs,
) -> (Vec<FeatureGeom>, ReassemblyStats) {
    let mut stats = ReassemblyStats {
        features_in: geoms.len() as u64,
        ..Default::default()
    };

    // collapsed_arc_count: independent of any ring. counted once per arc.
    for arc in &simp.arcs {
        if arc.len() == 2 {
            stats.collapsed_arc_count += 1;
        }
    }

    let mut cursor = 0usize;
    let mut out: Vec<FeatureGeom> = Vec::with_capacity(geoms.len());

    for f in geoms {
        match &f.geom {
            GeomKind::Polygon(orig) => {
                let rings = reassemble_polygon(orig, topo, simp, &mut cursor, &mut stats);
                out.push(FeatureGeom {
                    user_id: f.user_id,
                    bbox: f.bbox,
                    geom: GeomKind::Polygon(rings),
                });
            }
            GeomKind::MultiPolygon(parts) => {
                let mut new_parts: Vec<Vec<Vec<Coord>>> = Vec::with_capacity(parts.len());
                for orig in parts {
                    let rings = reassemble_polygon(orig, topo, simp, &mut cursor, &mut stats);
                    new_parts.push(rings);
                }
                out.push(FeatureGeom {
                    user_id: f.user_id,
                    bbox: f.bbox,
                    geom: GeomKind::MultiPolygon(new_parts),
                });
            }
            other => {
                // build_topology rejects non-polygon; passthrough for
                // robustness if the caller forgot to filter.
                out.push(FeatureGeom {
                    user_id: f.user_id,
                    bbox: f.bbox,
                    geom: other.clone(),
                });
            }
        }
        stats.features_out += 1;
    }

    (out, stats)
}

fn reassemble_polygon(
    orig: &[Vec<Coord>],
    topo: &Topology,
    simp: &SimplifiedArcs,
    cursor: &mut usize,
    stats: &mut ReassemblyStats,
) -> Vec<Vec<Coord>> {
    let mut new_rings: Vec<Vec<Coord>> = Vec::with_capacity(orig.len());
    for orig_ring in orig {
        let ring_arcs = &topo.rings[*cursor];
        *cursor += 1;
        stats.rings_total += 1;
        let mut simplified = reassemble_ring(simp, ring_arcs);
        if simplified.len() < 4 {
            stats.collapsed_ring_count += 1;
            simplified = orig_ring.clone();
        }
        if has_self_intersection(&simplified) {
            stats.self_intersection_count += 1;
        }
        new_rings.push(simplified);
    }
    // hole-in-shell check (cheap: first vertex of each hole inside shell?)
    if let Some(shell) = new_rings.first() {
        let shell_clone = shell.clone();
        for hole in new_rings.iter().skip(1) {
            if let Some(p) = hole.first()
                && !point_in_polygon(*p, &shell_clone)
            {
                stats.invalid_reassembly_count += 1;
            }
        }
    }
    new_rings
}

fn reassemble_ring(simp: &SimplifiedArcs, ring_arcs: &RingArcs) -> Vec<Coord> {
    if ring_arcs.pieces.is_empty() {
        return Vec::new();
    }
    if ring_arcs.island {
        // single closed arc; simplified already has first == last.
        if let Some((arc_id, _)) = ring_arcs.pieces.first()
            && let Some(arc) = simp.arcs.get(*arc_id as usize)
        {
            return arc.clone();
        }
        return Vec::new();
    }
    let mut out: Vec<Coord> = Vec::new();
    for (arc_id, dir) in &ring_arcs.pieces {
        let Some(arc) = simp.arcs.get(*arc_id as usize) else {
            continue;
        };
        match dir {
            Direction::Forward => append_arc(&mut out, arc.iter().copied()),
            Direction::Reverse => append_arc(&mut out, arc.iter().rev().copied()),
        }
    }
    // ensure closed (the last junction equals the first; dedup-on-append
    // already handled boundaries). force-close if floating-point drift left
    // them slightly different — shouldn't happen with first-writer-wins
    // canonical coords but cheap to enforce.
    if let (Some(&first), Some(&last)) = (out.first(), out.last())
        && first != last
    {
        out.push(first);
    }
    out
}

fn append_arc(out: &mut Vec<Coord>, mut iter: impl Iterator<Item = Coord>) {
    let Some(first) = iter.next() else { return };
    if out.is_empty() {
        out.push(first);
    } else if let Some(&last) = out.last()
        && last != first
    {
        // canonical-vertex-id boundary should match exactly; if it doesn't
        // (eg. island fallback splicing), still emit the boundary vertex.
        out.push(first);
    }
    for c in iter {
        out.push(c);
    }
}

/// ray-casting point-in-polygon. assumes a single closed ring (first == last).
/// boundary cases are not split out — for the seam-preservation gate we only
/// need a yes/no on hole containment after simplification.
fn point_in_polygon(p: Coord, ring: &[Coord]) -> bool {
    if ring.len() < 3 {
        return false;
    }
    let (px, py) = p;
    let mut inside = false;
    let n = ring.len();
    for i in 0..n {
        let (x1, y1) = ring[i];
        let (x2, y2) = ring[(i + 1) % n];
        let crosses = (y1 > py) != (y2 > py) && {
            let denom = y2 - y1;
            if denom == 0.0 {
                false
            } else {
                let xi = x1 + (py - y1) * (x2 - x1) / denom;
                px < xi
            }
        };
        if crosses {
            inside = !inside;
        }
    }
    inside
}

/// O(n²) self-intersection scan. proper crossings only — shared endpoints
/// between adjacent segments don't count. spike scale: rings here have ≤
/// a few hundred verts after simplification.
fn has_self_intersection(ring: &[Coord]) -> bool {
    if ring.len() < 4 {
        return false;
    }
    // segments are ring[i]..ring[i+1] for i in 0..ring.len()-1. ring is closed.
    let n = ring.len() - 1;
    for i in 0..n {
        let a = ring[i];
        let b = ring[i + 1];
        for j in (i + 2)..n {
            // skip the segment that shares an endpoint with segment i in the
            // closed ring (segment 0 and segment n-1 share the closure point).
            if i == 0 && j == n - 1 {
                continue;
            }
            let c = ring[j];
            let d = ring[j + 1];
            if segments_properly_cross(a, b, c, d) {
                return true;
            }
        }
    }
    false
}

#[inline]
fn segments_properly_cross(p1: Coord, p2: Coord, p3: Coord, p4: Coord) -> bool {
    let d1 = orient(p3, p4, p1);
    let d2 = orient(p3, p4, p2);
    let d3 = orient(p1, p2, p3);
    let d4 = orient(p1, p2, p4);
    ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0)) && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
}

#[inline]
fn orient(a: Coord, b: Coord, c: Coord) -> f64 {
    (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::dp::simplify_arcs;
    use crate::graph::build_topology;

    fn poly(id: u64, ring: Vec<Coord>) -> FeatureGeom {
        FeatureGeom {
            user_id: id,
            bbox: [0.0, 0.0, 0.0, 0.0],
            geom: GeomKind::Polygon(vec![ring]),
        }
    }

    #[test]
    fn shared_seam_simplification_keeps_both_polygons_aligned() {
        // P1 + P2 share the seam (10,0)-(10,5)-(10,10). At tol=0.5 the
        // collinear midpoint drops; both reassembled polygons must show the
        // simplified seam (no (10,5)) and remain identical along the seam.
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
        let geoms = vec![p1, p2];
        let (topo, _) = build_topology(&geoms, 1);
        let simp = simplify_arcs(&topo, 0.5);
        let (out, stats) = reassemble(&geoms, &topo, &simp);
        assert_eq!(stats.features_out, 2);
        assert_eq!(stats.collapsed_ring_count, 0);
        assert_eq!(stats.invalid_reassembly_count, 0);
        assert_eq!(stats.self_intersection_count, 0);
        // each ring went from 6 vertices down to 5 (collinear midpoint dropped)
        for f in &out {
            match &f.geom {
                GeomKind::Polygon(rings) => assert_eq!(rings[0].len(), 5),
                _ => panic!("expected polygon"),
            }
        }
        // verify the seam is identical between the two simplified polygons.
        let seam1: Vec<Coord> = match &out[0].geom {
            GeomKind::Polygon(rings) => rings[0]
                .iter()
                .copied()
                .filter(|(x, _)| (*x - 10.0).abs() < 1e-9)
                .collect(),
            _ => panic!(),
        };
        let seam2: Vec<Coord> = match &out[1].geom {
            GeomKind::Polygon(rings) => rings[0]
                .iter()
                .copied()
                .filter(|(x, _)| (*x - 10.0).abs() < 1e-9)
                .collect(),
            _ => panic!(),
        };
        // they should contain exactly the same set of seam vertices
        let s1: std::collections::HashSet<(i64, i64)> = seam1
            .iter()
            .map(|(x, y)| ((*x * 1000.0) as i64, (*y * 1000.0) as i64))
            .collect();
        let s2: std::collections::HashSet<(i64, i64)> = seam2
            .iter()
            .map(|(x, y)| ((*x * 1000.0) as i64, (*y * 1000.0) as i64))
            .collect();
        assert_eq!(s1, s2);
        assert!(s1.contains(&(10_000, 0)));
        assert!(s1.contains(&(10_000, 10_000)));
        assert!(!s1.contains(&(10_000, 5_000)));
    }

    #[test]
    fn island_polygon_round_trip() {
        let ring = vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)];
        let p = poly(1, ring);
        let geoms = vec![p];
        let (topo, _) = build_topology(&geoms, 1);
        let simp = simplify_arcs(&topo, 0.5);
        let (out, stats) = reassemble(&geoms, &topo, &simp);
        assert_eq!(stats.features_out, 1);
        match &out[0].geom {
            GeomKind::Polygon(rings) => {
                assert_eq!(rings.len(), 1);
                assert_eq!(rings[0].first(), rings[0].last());
                // square has no collinear midpoints; vertex count stays 5.
                assert_eq!(rings[0].len(), 5);
            }
            _ => panic!("expected polygon"),
        }
    }

    #[test]
    fn polygon_with_hole_round_trip() {
        let shell = vec![(0.0, 0.0), (100.0, 0.0), (100.0, 100.0), (0.0, 100.0), (0.0, 0.0)];
        let hole = vec![(20.0, 20.0), (80.0, 20.0), (80.0, 80.0), (20.0, 80.0), (20.0, 20.0)];
        let f = FeatureGeom {
            user_id: 1,
            bbox: [0.0, 0.0, 100.0, 100.0],
            geom: GeomKind::Polygon(vec![shell, hole]),
        };
        let geoms = vec![f];
        let (topo, _) = build_topology(&geoms, 1);
        let simp = simplify_arcs(&topo, 0.5);
        let (out, stats) = reassemble(&geoms, &topo, &simp);
        assert_eq!(stats.invalid_reassembly_count, 0);
        match &out[0].geom {
            GeomKind::Polygon(rings) => {
                assert_eq!(rings.len(), 2);
                assert_eq!(rings[0].len(), 5);
                assert_eq!(rings[1].len(), 5);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn point_in_polygon_basic() {
        let sq = vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)];
        assert!(point_in_polygon((5.0, 5.0), &sq));
        assert!(!point_in_polygon((-1.0, 5.0), &sq));
        assert!(!point_in_polygon((5.0, 11.0), &sq));
    }

    #[test]
    fn self_intersection_detects_bowtie() {
        // bowtie: (0,0)-(10,10)-(10,0)-(0,10)-(0,0)
        let bowtie = vec![(0.0, 0.0), (10.0, 10.0), (10.0, 0.0), (0.0, 10.0), (0.0, 0.0)];
        assert!(has_self_intersection(&bowtie));
        let square = vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)];
        assert!(!has_self_intersection(&square));
    }
}
