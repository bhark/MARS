//! seam-preservation verifier.
//!
//! the spike's primary correctness gate: every shared arc detected at the
//! graph stage must, after simplification + reassembly, still appear as a
//! contiguous subsequence in every ring that referenced it (in the
//! ring-specific traversal direction). if a ring fell back to its original
//! coords because simplification collapsed it, the simplified shared arc
//! will be missing on that side — that's a seam violation.
//!
//! the substring check is per-arc per-referencing-ring; for spike scale
//! (ring length << thousands, shared arcs typically two refs each), the
//! cost is well below the DP and graph stages.

use mars_artifact::{Coord, FeatureGeom, GeomKind};

use crate::dp::SimplifiedArcs;
use crate::graph::{Direction, RingHandle, Topology};

#[derive(Debug, Default, Clone)]
pub struct SeamStats {
    pub shared_arc_count: u64,
    pub seam_violation_count: u64,
    /// breakdown for diagnostics: how many violations were due to a ring
    /// having lost the arc entirely (the common case after fallback).
    pub violation_missing_subsequence: u64,
}

pub fn verify_seams(topo: &Topology, simp: &SimplifiedArcs, reassembled: &[FeatureGeom]) -> SeamStats {
    let mut stats = SeamStats::default();
    // arc_id -> Vec<(RingHandle, Direction)>
    let mut refs: Vec<Vec<(RingHandle, Direction)>> = vec![Vec::new(); topo.arcs.len()];
    for ring_arcs in &topo.rings {
        for (arc_id, dir) in &ring_arcs.pieces {
            if let Some(slot) = refs.get_mut(*arc_id as usize) {
                slot.push((ring_arcs.handle, *dir));
            }
        }
    }

    for (arc_id, arc) in topo.arcs.iter().enumerate() {
        if arc.shared_count < 2 {
            continue;
        }
        stats.shared_arc_count += 1;
        let arc_seq = match simp.arcs.get(arc_id) {
            Some(seq) => seq,
            None => continue,
        };
        let arc_refs = &refs[arc_id];
        for (handle, dir) in arc_refs {
            let Some(ring) = lookup_ring(reassembled, *handle) else {
                stats.seam_violation_count += 1;
                stats.violation_missing_subsequence += 1;
                continue;
            };
            let expected: Vec<Coord> = match dir {
                Direction::Forward => arc_seq.clone(),
                Direction::Reverse => arc_seq.iter().rev().copied().collect(),
            };
            if !contains_subsequence(ring, &expected) {
                stats.seam_violation_count += 1;
                stats.violation_missing_subsequence += 1;
            }
        }
    }
    stats
}

fn lookup_ring(reassembled: &[FeatureGeom], handle: RingHandle) -> Option<&[Coord]> {
    let feat = reassembled.get(handle.feature_idx as usize)?;
    match &feat.geom {
        GeomKind::Polygon(rings) => rings.get(handle.ring_idx as usize).map(Vec::as_slice),
        GeomKind::MultiPolygon(parts) => parts
            .get(handle.part_idx as usize)
            .and_then(|polygon| polygon.get(handle.ring_idx as usize).map(Vec::as_slice)),
        _ => None,
    }
}

/// contiguous substring search with cyclic wrap. ring is closed (first == last)
/// so we treat it as cyclic over the open form (length n-1). expected length
/// must be <= ring open length + 1 to match.
fn contains_subsequence(ring: &[Coord], expected: &[Coord]) -> bool {
    if expected.is_empty() {
        return true;
    }
    if ring.len() < expected.len() {
        return false;
    }
    // straight match
    for i in 0..=(ring.len() - expected.len()) {
        if ring[i..i + expected.len()] == *expected {
            return true;
        }
    }
    // cyclic wrap: ring is closed. open form length = ring.len() - 1.
    // for arcs that span the start/end seam, drop the closing duplicate
    // and look at indices that wrap.
    if ring.len() < 2 {
        return false;
    }
    let open_len = ring.len() - 1;
    if expected.len() > open_len {
        return false;
    }
    for start in 0..open_len {
        let mut all_match = true;
        for k in 0..expected.len() {
            let r = ring[(start + k) % open_len];
            if r != expected[k] {
                all_match = false;
                break;
            }
        }
        if all_match {
            return true;
        }
    }
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::dp::simplify_arcs;
    use crate::graph::build_topology;
    use crate::reassemble::reassemble;

    fn poly(id: u64, ring: Vec<Coord>) -> FeatureGeom {
        FeatureGeom {
            user_id: id,
            bbox: [0.0, 0.0, 0.0, 0.0],
            geom: GeomKind::Polygon(vec![ring]),
        }
    }

    #[test]
    fn shared_seam_is_preserved_after_simplification() {
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
        let (out, _) = reassemble(&geoms, &topo, &simp);
        let stats = verify_seams(&topo, &simp, &out);
        assert_eq!(stats.shared_arc_count, 1);
        assert_eq!(stats.seam_violation_count, 0);
    }

    #[test]
    fn island_polygon_has_no_shared_arcs() {
        let p = poly(1, vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)]);
        let geoms = vec![p];
        let (topo, _) = build_topology(&geoms, 1);
        let simp = simplify_arcs(&topo, 0.5);
        let (out, _) = reassemble(&geoms, &topo, &simp);
        let stats = verify_seams(&topo, &simp, &out);
        assert_eq!(stats.shared_arc_count, 0);
        assert_eq!(stats.seam_violation_count, 0);
    }

    #[test]
    fn ring_missing_seam_triggers_violation() {
        // simulate the asymmetric-fallback case: reassembly produced a valid
        // simplified ring on P1 but P2 fell back to a ring that no longer
        // contains the simplified arc. directly mutate the reassembled output
        // since organic fallback only fires below the 4-vertex floor and
        // engineering that case symmetrically is fiddly — the verifier's job
        // is to catch a missing subsequence regardless of how it got missing.
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
        let (mut out, _) = reassemble(&geoms, &topo, &simp);
        // tampering: replace P2's ring with a shape that lacks the seam.
        out[1].geom = GeomKind::Polygon(vec![vec![
            (50.0, 50.0),
            (60.0, 50.0),
            (60.0, 60.0),
            (50.0, 60.0),
            (50.0, 50.0),
        ]]);
        let stats = verify_seams(&topo, &simp, &out);
        assert_eq!(stats.shared_arc_count, 1);
        assert_eq!(stats.seam_violation_count, 1);
    }

    #[test]
    fn substring_finds_cyclic_match() {
        let ring = vec![(1.0, 0.0), (2.0, 0.0), (3.0, 0.0), (1.0, 0.0)];
        // wrap from index 2 -> 0 -> 1 = (3,0),(1,0),(2,0) is actually a
        // legitimate cyclic match in the open form (length 3).
        assert!(contains_subsequence(&ring, &[(3.0, 0.0), (1.0, 0.0), (2.0, 0.0)]));
        // straight match
        assert!(contains_subsequence(&ring, &[(1.0, 0.0), (2.0, 0.0), (3.0, 0.0)]));
        // missing
        assert!(!contains_subsequence(&ring, &[(7.0, 7.0), (8.0, 8.0)]));
    }
}
