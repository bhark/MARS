//! boundary graph: quantise vertices, build per-edge ring membership, detect
//! junctions, split rings into canonical arcs.
//!
//! a vertex is a junction iff it has != 2 distinct incident undirected edges
//! OR its two incident edges have different ring-id sets. arcs are maximal
//! runs of non-junction interior vertices between junctions; opposite-direction
//! arcs from neighbouring rings collapse to the same canonical id by
//! lex-minimising the vertex-id sequence.
//!
//! closed-loop rings (no junctions on them — island polygons) get their own
//! unique arc id with no canonicalisation; they aren't shared with anyone, so
//! the dedup machinery would just be ceremony for them.

// fields below are consumed by the dp/reassemble/verify modules landing in
// later commits; suppress dead-code until those wire in.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use mars_artifact::{Coord, FeatureGeom, GeomKind};

/// addressable handle for a single ring inside the input geometry set.
/// part_idx/ring_idx are 0 for non-multi geometries.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct RingHandle {
    pub feature_idx: u32,
    pub part_idx: u32,
    pub ring_idx: u32,
}

/// canonical arc id (index into [`Topology::arcs`]).
pub type ArcId = u32;

/// vertex id (index into [`Topology::vertices`]).
pub type VertexId = u32;

/// quantised coord key used for vertex deduplication.
pub type QCoord = (i64, i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Reverse,
}

#[derive(Debug, Clone)]
pub struct RingArcs {
    pub handle: RingHandle,
    /// pieces in original-ring traversal order; closed (last junction == first)
    pub pieces: Vec<(ArcId, Direction)>,
    /// true iff the ring had no junctions and is represented by a single
    /// island arc (whose canonical sequence equals the ring itself).
    pub island: bool,
}

#[derive(Debug, Clone)]
pub struct Arc {
    /// canonical-direction vertex-id sequence (length >= 2). first and last
    /// are junctions for shared arcs; for islands they are equal.
    pub canonical: Vec<VertexId>,
    /// number of distinct rings that reference this arc. shared seam arcs
    /// have shared_count >= 2; per-ring boundary arcs and islands have 1.
    pub shared_count: u32,
}

#[derive(Debug)]
pub struct Topology {
    /// canonical un-quantised coord per vertex (first writer wins).
    pub vertices: Vec<Coord>,
    /// arcs in canonical direction.
    pub arcs: Vec<Arc>,
    /// one entry per input ring (in input order, polygon parts flattened).
    pub rings: Vec<RingArcs>,
}

#[derive(Debug, Default, Clone)]
pub struct GraphStats {
    pub feature_count: u64,
    pub ring_count: u64,
    pub vertex_count: u64,
    pub edge_count: u64,
    pub junction_count: u64,
    pub arc_count: u64,
    pub island_arc_count: u64,
    pub shared_arc_count: u64,
}

/// build a [`Topology`] from polygon / multipolygon features. quantisation grid
/// is in millimetres (canonical-CRS units); `1` keeps full mm precision and is
/// the default. non-polygon geometries should be filtered out by the caller —
/// passing one here panics in debug, is silently ignored in release.
pub fn build_topology(geoms: &[FeatureGeom], quantise_mm: u32) -> (Topology, GraphStats) {
    let mut interner = VertexInterner::new(quantise_mm);
    let mut rings_raw: Vec<(RingHandle, Vec<VertexId>)> = Vec::new();

    for (fi, f) in geoms.iter().enumerate() {
        let fi = fi as u32;
        match &f.geom {
            GeomKind::Polygon(polygon) => {
                for (ri, ring) in polygon.iter().enumerate() {
                    push_ring(
                        &mut rings_raw,
                        &mut interner,
                        RingHandle {
                            feature_idx: fi,
                            part_idx: 0,
                            ring_idx: ri as u32,
                        },
                        ring,
                    );
                }
            }
            GeomKind::MultiPolygon(parts) => {
                for (pi, polygon) in parts.iter().enumerate() {
                    for (ri, ring) in polygon.iter().enumerate() {
                        push_ring(
                            &mut rings_raw,
                            &mut interner,
                            RingHandle {
                                feature_idx: fi,
                                part_idx: pi as u32,
                                ring_idx: ri as u32,
                            },
                            ring,
                        );
                    }
                }
            }
            _ => {
                debug_assert!(false, "non-polygon fed to build_topology");
            }
        }
    }

    // edges and per-vertex incident-edge index. per-edge ring set is what the
    // junction rule consults. EdgeKey is (min, max) vertex ids.
    let mut edge_rings: HashMap<(VertexId, VertexId), Vec<u32>> = HashMap::new();
    let mut vertex_edges: HashMap<VertexId, HashSet<(VertexId, VertexId)>> = HashMap::new();

    for (ring_id, (_, ring)) in rings_raw.iter().enumerate() {
        let ring_id = ring_id as u32;
        for w in ring.windows(2) {
            let (a, b) = (w[0], w[1]);
            if a == b {
                // zero-length edge (collapsed by quantisation); skip.
                continue;
            }
            let key = if a < b { (a, b) } else { (b, a) };
            edge_rings.entry(key).or_default().push(ring_id);
            vertex_edges.entry(a).or_default().insert(key);
            vertex_edges.entry(b).or_default().insert(key);
        }
    }
    // dedupe ring lists per edge so a ring that traverses the same edge twice
    // (rare, only via degenerate self-touch) still counts as one ring.
    for set in edge_rings.values_mut() {
        set.sort_unstable();
        set.dedup();
    }

    let mut junctions: HashSet<VertexId> = HashSet::new();
    for (vid, incident) in &vertex_edges {
        if incident.len() != 2 {
            junctions.insert(*vid);
            continue;
        }
        let mut iter = incident.iter();
        let e1 = iter.next().copied().unwrap_or((0, 0));
        let e2 = iter.next().copied().unwrap_or((0, 0));
        let r1 = edge_rings.get(&e1).map(Vec::as_slice).unwrap_or(&[]);
        let r2 = edge_rings.get(&e2).map(Vec::as_slice).unwrap_or(&[]);
        if r1 != r2 {
            junctions.insert(*vid);
        }
    }

    // arc extraction.
    let mut arcs: Vec<Arc> = Vec::new();
    let mut arc_key_index: HashMap<Vec<VertexId>, ArcId> = HashMap::new();
    let mut rings_out: Vec<RingArcs> = Vec::with_capacity(rings_raw.len());

    for (handle, ring) in rings_raw {
        if ring.len() < 2 {
            // degenerate; emit empty record so feature index alignment holds.
            rings_out.push(RingArcs {
                handle,
                pieces: Vec::new(),
                island: false,
            });
            continue;
        }
        // ring is closed (first == last). work on the open form.
        let open_len = ring.len() - 1;
        // find first junction in the ring
        let first_junction = (0..open_len).find(|i| junctions.contains(&ring[*i]));
        match first_junction {
            None => {
                // island ring: whole loop is one arc, no canonicalisation.
                let mut canonical = ring[..open_len].to_vec();
                // close it explicitly so reassembly always sees first == last.
                canonical.push(canonical[0]);
                let arc_id = arcs.len() as u32;
                arcs.push(Arc {
                    canonical,
                    shared_count: 1,
                });
                rings_out.push(RingArcs {
                    handle,
                    pieces: vec![(arc_id, Direction::Forward)],
                    island: true,
                });
            }
            Some(start) => {
                // rotate so ring starts at a junction; collect arcs.
                let mut pieces: Vec<(ArcId, Direction)> = Vec::new();
                let mut cur: Vec<VertexId> = Vec::with_capacity(8);
                cur.push(ring[start]);
                let mut i = start;
                loop {
                    let next = (i + 1) % open_len;
                    cur.push(ring[next]);
                    if junctions.contains(&ring[next]) {
                        // close this arc.
                        let (arc_id, dir) = intern_open_arc(&mut arcs, &mut arc_key_index, &cur);
                        pieces.push((arc_id, dir));
                        if next == start {
                            break;
                        }
                        cur.clear();
                        cur.push(ring[next]);
                    }
                    i = next;
                }
                rings_out.push(RingArcs {
                    handle,
                    pieces,
                    island: false,
                });
            }
        }
    }

    let stats = GraphStats {
        feature_count: geoms.len() as u64,
        ring_count: rings_out.len() as u64,
        vertex_count: interner.coords.len() as u64,
        edge_count: edge_rings.len() as u64,
        junction_count: junctions.len() as u64,
        arc_count: arcs.len() as u64,
        island_arc_count: rings_out.iter().filter(|r| r.island).count() as u64,
        shared_arc_count: arcs.iter().filter(|a| a.shared_count >= 2).count() as u64,
    };

    (
        Topology {
            vertices: interner.coords,
            arcs,
            rings: rings_out,
        },
        stats,
    )
}

/// canonicalise an open arc by lex-minimising direction. updates shared_count
/// when an existing arc is hit. `seq` is the ring's traversal of this arc;
/// length >= 2, first and last are junctions.
fn intern_open_arc(
    arcs: &mut Vec<Arc>,
    index: &mut HashMap<Vec<VertexId>, ArcId>,
    seq: &[VertexId],
) -> (ArcId, Direction) {
    let mut rev: Vec<VertexId> = seq.iter().rev().copied().collect();
    let canonical_dir;
    let key: Vec<VertexId> = if seq <= rev.as_slice() {
        canonical_dir = Direction::Forward;
        seq.to_vec()
    } else {
        canonical_dir = Direction::Reverse;
        std::mem::take(&mut rev)
    };
    if let Some(&id) = index.get(&key) {
        arcs[id as usize].shared_count = arcs[id as usize].shared_count.saturating_add(1);
        return (id, canonical_dir);
    }
    let id = arcs.len() as u32;
    arcs.push(Arc {
        canonical: key.clone(),
        shared_count: 1,
    });
    index.insert(key, id);
    (id, canonical_dir)
}

fn push_ring(
    out: &mut Vec<(RingHandle, Vec<VertexId>)>,
    interner: &mut VertexInterner,
    handle: RingHandle,
    ring: &[Coord],
) {
    if ring.len() < 2 {
        out.push((handle, Vec::new()));
        return;
    }
    let mut vids: Vec<VertexId> = Vec::with_capacity(ring.len());
    for &(x, y) in ring {
        vids.push(interner.intern(x, y));
    }
    // ensure closed; some sources omit the closing duplicate.
    if vids.first() != vids.last()
        && let Some(&first) = vids.first()
    {
        vids.push(first);
    }
    out.push((handle, vids));
}

struct VertexInterner {
    table: HashMap<QCoord, VertexId>,
    coords: Vec<Coord>,
    grid_mm: f64,
}

impl VertexInterner {
    fn new(quantise_mm: u32) -> Self {
        let g = if quantise_mm == 0 { 1 } else { quantise_mm };
        Self {
            table: HashMap::new(),
            coords: Vec::new(),
            grid_mm: f64::from(g),
        }
    }
    fn intern(&mut self, x: f64, y: f64) -> VertexId {
        // quantise to multiples of grid_mm millimetres in canonical units.
        // input coords are metres; multiply by 1000/grid_mm and round.
        let scale = 1000.0 / self.grid_mm;
        let qx = (x * scale).round() as i64;
        let qy = (y * scale).round() as i64;
        let key = (qx, qy);
        if let Some(&id) = self.table.get(&key) {
            return id;
        }
        let id = self.coords.len() as u32;
        self.coords.push((x, y));
        self.table.insert(key, id);
        id
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use mars_artifact::FeatureGeom;

    fn poly(id: u64, ring: Vec<Coord>) -> FeatureGeom {
        FeatureGeom {
            user_id: id,
            bbox: [0.0, 0.0, 0.0, 0.0],
            geom: GeomKind::Polygon(vec![ring]),
        }
    }

    #[test]
    fn island_polygon_yields_one_unshared_arc() {
        let p = poly(1, vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)]);
        let (topo, stats) = build_topology(&[p], 1);
        assert_eq!(stats.feature_count, 1);
        assert_eq!(stats.ring_count, 1);
        assert_eq!(stats.junction_count, 0);
        assert_eq!(stats.arc_count, 1);
        assert_eq!(stats.island_arc_count, 1);
        assert!(topo.rings[0].island);
        assert_eq!(topo.rings[0].pieces.len(), 1);
        assert_eq!(topo.arcs[0].shared_count, 1);
    }

    #[test]
    fn two_squares_sharing_one_edge_share_one_arc() {
        // P1: 0,0 -> 10,0 -> 10,10 -> 0,10 -> 0,0 (right edge x=10 is shared)
        // P2: 10,0 -> 20,0 -> 20,10 -> 10,10 -> 10,0 (left edge x=10 is shared)
        let p1 = poly(1, vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)]);
        let p2 = poly(
            2,
            vec![(10.0, 0.0), (20.0, 0.0), (20.0, 10.0), (10.0, 10.0), (10.0, 0.0)],
        );
        let (topo, stats) = build_topology(&[p1, p2], 1);
        assert_eq!(stats.ring_count, 2);
        // junctions: (10,0) and (10,10); the four exterior corners are not.
        assert_eq!(stats.junction_count, 2);
        // arcs: the shared seam (10,0)->(10,10), the L-shape of P1 around it
        // back to (10,0), and the L-shape of P2. 3 arcs total.
        assert_eq!(stats.arc_count, 3);
        assert_eq!(stats.shared_arc_count, 1);
        // each ring traverses 2 arcs (the L plus the seam).
        assert_eq!(topo.rings[0].pieces.len(), 2);
        assert_eq!(topo.rings[1].pieces.len(), 2);
        // the shared arc must appear in both rings' piece lists.
        let shared_ids: HashSet<ArcId> = topo
            .arcs
            .iter()
            .enumerate()
            .filter_map(|(i, a)| if a.shared_count >= 2 { Some(i as ArcId) } else { None })
            .collect();
        let p1_arcs: HashSet<ArcId> = topo.rings[0].pieces.iter().map(|(a, _)| *a).collect();
        let p2_arcs: HashSet<ArcId> = topo.rings[1].pieces.iter().map(|(a, _)| *a).collect();
        for sid in &shared_ids {
            assert!(p1_arcs.contains(sid));
            assert!(p2_arcs.contains(sid));
        }
    }

    #[test]
    fn shared_arc_directions_are_opposite() {
        let p1 = poly(1, vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)]);
        let p2 = poly(
            2,
            vec![(10.0, 0.0), (20.0, 0.0), (20.0, 10.0), (10.0, 10.0), (10.0, 0.0)],
        );
        let (topo, _) = build_topology(&[p1, p2], 1);
        let shared_id = topo
            .arcs
            .iter()
            .enumerate()
            .find_map(|(i, a)| if a.shared_count >= 2 { Some(i as ArcId) } else { None })
            .unwrap();
        let dir1 = topo.rings[0].pieces.iter().find(|(a, _)| *a == shared_id).unwrap().1;
        let dir2 = topo.rings[1].pieces.iter().find(|(a, _)| *a == shared_id).unwrap().1;
        assert_ne!(dir1, dir2);
    }

    #[test]
    fn t_junction_unnoded_does_not_share() {
        // P1's right edge is split into two segments at (10, 5).
        // P2's left edge is one segment from (10,0) to (10,10), no break at 5.
        // expectation: no arc is shared; (10,5) is a junction in P1's graph
        // because its two incident edges have ring-set {P1} but P2 doesn't
        // visit (10,5) → only P1 has incident edges there. it ends up
        // non-junction (incident edges 2 with same ring set {P1}). the seam
        // on P2 is one undirected edge {(10,0),(10,10)}, on P1 it is two
        // edges {(10,0),(10,5)} and {(10,5),(10,10)}. so the edge sets don't
        // align and there is NO shared arc. seam-violation territory; here
        // assert the topology reflects it.
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
            vec![(10.0, 0.0), (20.0, 0.0), (20.0, 10.0), (10.0, 10.0), (10.0, 0.0)],
        );
        let (topo, stats) = build_topology(&[p1, p2], 1);
        assert_eq!(stats.shared_arc_count, 0);
        assert!(!topo.arcs.is_empty());
    }
}
