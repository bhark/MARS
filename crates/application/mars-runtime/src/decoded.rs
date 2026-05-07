//! decoded source-artifact geometry + bytes-bounded LRU cache.
//!
//! the geometry-payload codec walks LEB128 varints from byte zero on every
//! render, so the same source artifact decodes once per request even when its
//! bytes haven't changed. SPEC §10.4 reserves a "bounded process-memory cache
//! (decoded hot chunks only)" tier; this module is the geometry instance of
//! it. content hashes are immutable, so a cache hit is unconditionally valid
//! for the lifetime of the bytes they address.

use std::sync::Arc;

use hashlink::LinkedHashMap;
use mars_artifact::{
    ArtifactError, ArtifactReader, GeomType, GeomVisitor, SectionKind, iter_feature_index, visit_one_geom,
};
use mars_types::ContentHash;
use parking_lot::Mutex;

use crate::RuntimeError;

/// one feature's decoded geometry in canonical CRS.
#[derive(Debug)]
pub(crate) struct DecodedFeature {
    pub id: u64,
    pub bbox: [f32; 4],
    pub geom_type: GeomType,
    /// canonical-CRS coords organised ring-by-ring. for Point/MultiPoint each
    /// "ring" is a single-coord vector representing one standalone vertex; for
    /// LineString/Polygon/Multi-* each ring is a normal coord sequence.
    pub rings: Vec<Vec<[f64; 2]>>,
}

/// decoded view of a source artifact's geometry payload.
#[derive(Debug)]
pub(crate) struct DecodedSourceGeometry {
    pub features: Vec<DecodedFeature>,
    /// approximate bytes the entry occupies, used for cache accounting.
    pub bytes: usize,
}

const FEATURE_FIXED_OVERHEAD: usize = std::mem::size_of::<DecodedFeature>();
const RING_FIXED_OVERHEAD: usize = std::mem::size_of::<Vec<[f64; 2]>>();
const COORD_BYTES: usize = std::mem::size_of::<[f64; 2]>();

impl DecodedSourceGeometry {
    fn empty() -> Self {
        Self {
            features: Vec::new(),
            bytes: 0,
        }
    }

    fn record_bytes(&mut self) {
        let mut b: usize = 0;
        for f in &self.features {
            b = b.saturating_add(FEATURE_FIXED_OVERHEAD);
            for r in &f.rings {
                b = b.saturating_add(RING_FIXED_OVERHEAD);
                b = b.saturating_add(r.len().saturating_mul(COORD_BYTES));
            }
        }
        self.bytes = b;
    }
}

/// decode the geometry-payload section of a source artifact into the
/// renderer-shaped form. raster-only sources lack a geometry payload; that
/// case returns an empty result.
pub(crate) fn decode_source_geometry(reader: &ArtifactReader) -> Result<DecodedSourceGeometry, RuntimeError> {
    let geom_section = match reader.section(SectionKind::GeometryPayload) {
        Ok(b) => b,
        Err(ArtifactError::SectionMissing(_)) => return Ok(DecodedSourceGeometry::empty()),
        Err(e) => return Err(e.into()),
    };
    let iter = iter_feature_index(&geom_section)?;
    let coord_area = iter.coord_area();
    let mut features = Vec::with_capacity(iter.len());
    for entry in iter {
        let entry = entry?;
        let geom_type = entry.geom_kind()?;
        let mut visitor = DecodeVisitor {
            rings: Vec::new(),
            in_ring: false,
        };
        visit_one_geom(coord_area, &entry, &mut visitor)?;
        features.push(DecodedFeature {
            id: entry.id,
            bbox: entry.bbox,
            geom_type,
            rings: visitor.rings,
        });
    }
    let mut decoded = DecodedSourceGeometry { features, bytes: 0 };
    decoded.record_bytes();
    Ok(decoded)
}

struct DecodeVisitor {
    rings: Vec<Vec<[f64; 2]>>,
    in_ring: bool,
}

impl GeomVisitor for DecodeVisitor {
    #[inline]
    fn point(&mut self, x: f64, y: f64) {
        if self.in_ring {
            // begin_ring is invariably called before any in-ring point. last_mut
            // can only be None if the codec emitted point() without a preceding
            // begin_ring, which would be a codec bug; treating it as a fresh
            // standalone ring keeps us correctness-safe rather than panicking.
            match self.rings.last_mut() {
                Some(ring) => ring.push([x, y]),
                None => self.rings.push(vec![[x, y]]),
            }
        } else {
            // standalone vertex (Point / MultiPoint)
            self.rings.push(vec![[x, y]]);
        }
    }
    fn begin_ring(&mut self) {
        self.rings.push(Vec::new());
        self.in_ring = true;
    }
    fn end_ring(&mut self) {
        self.in_ring = false;
    }
    fn begin_part(&mut self) {}
    fn end_part(&mut self) {}
}

/// bytes-bounded LRU keyed by source-artifact content hash.
#[derive(Debug)]
pub struct DecodedGeometryCache {
    state: Mutex<CacheState>,
    max_bytes: usize,
}

#[derive(Debug)]
struct CacheState {
    lru: LinkedHashMap<ContentHash, Entry>,
    total_bytes: usize,
}

#[derive(Debug)]
struct Entry {
    value: Arc<DecodedSourceGeometry>,
    bytes: usize,
}

impl DecodedGeometryCache {
    /// Construct a cache bounded to `max_bytes` of decoded geometry. A cap of
    /// zero disables retention (every insert evicts on the next insert).
    #[must_use]
    pub fn new(max_bytes: usize) -> Self {
        Self {
            state: Mutex::new(CacheState {
                lru: LinkedHashMap::new(),
                total_bytes: 0,
            }),
            max_bytes,
        }
    }

    /// Total bytes currently retained. Test/observability only.
    #[must_use]
    pub fn current_bytes(&self) -> usize {
        self.state.lock().total_bytes
    }

    /// Look up a decoded entry by content hash. Bumps LRU recency on hit.
    pub(crate) fn get(&self, hash: &ContentHash) -> Option<Arc<DecodedSourceGeometry>> {
        let mut state = self.state.lock();
        let entry = state.lru.to_back(hash)?;
        Some(entry.value.clone())
    }

    /// Drop all retained entries. Diagnostics-only — used by the parity diff
    /// harness to measure cold-decode wall time across iterations.
    pub fn clear(&self) {
        let mut state = self.state.lock();
        state.lru.clear();
        state.total_bytes = 0;
    }

    /// Insert a decoded entry under `hash`. Evicts LRU entries until total
    /// bytes ≤ max_bytes; if a single entry exceeds the cap on its own, it is
    /// retained as the sole resident — eviction handles it on the next insert.
    pub(crate) fn insert(&self, hash: ContentHash, value: Arc<DecodedSourceGeometry>) {
        let bytes = value.bytes;
        let mut state = self.state.lock();
        if let Some(prev) = state.lru.insert(hash, Entry { value, bytes }) {
            state.total_bytes = state.total_bytes.saturating_sub(prev.bytes);
        }
        state.total_bytes = state.total_bytes.saturating_add(bytes);
        while state.total_bytes > self.max_bytes && state.lru.len() > 1 {
            let Some((_, evicted)) = state.lru.pop_front() else {
                break;
            };
            state.total_bytes = state.total_bytes.saturating_sub(evicted.bytes);
        }
    }
}

impl Default for DecodedGeometryCache {
    fn default() -> Self {
        Self::new(0)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use mars_artifact::{ArtifactKind, ArtifactWriter, FeatureGeom, GeomKind};
    use mars_types::Bbox;

    use super::*;

    fn build_source(features: Vec<FeatureGeom>) -> ArtifactReader {
        let mut w = ArtifactWriter::new(ArtifactKind::Source);
        let n = features.len() as u64;
        w.add_geometry_payload(features)
            .set_bbox(Bbox::new(0.0, 0.0, 10.0, 10.0))
            .set_feature_count(n);
        ArtifactReader::open(w.finish().unwrap()).unwrap()
    }

    #[test]
    fn decodes_polygon_into_rings() {
        let reader = build_source(vec![FeatureGeom {
            id: 1,
            bbox: [0.0, 0.0, 10.0, 10.0],
            geom: GeomKind::Polygon(vec![vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 0.0)]]),
        }]);
        let decoded = decode_source_geometry(&reader).unwrap();
        assert_eq!(decoded.features.len(), 1);
        let f = &decoded.features[0];
        assert!(matches!(f.geom_type, GeomType::Polygon));
        assert_eq!(f.rings.len(), 1);
        assert_eq!(f.rings[0].len(), 4);
        assert!(decoded.bytes > 0);
    }

    #[test]
    fn decodes_point_as_single_coord_ring() {
        let reader = build_source(vec![FeatureGeom {
            id: 1,
            bbox: [5.0, 5.0, 5.0, 5.0],
            geom: GeomKind::Point((5.0, 5.0)),
        }]);
        let decoded = decode_source_geometry(&reader).unwrap();
        assert_eq!(decoded.features.len(), 1);
        let f = &decoded.features[0];
        assert!(matches!(f.geom_type, GeomType::Point));
        assert_eq!(f.rings.len(), 1);
        assert_eq!(f.rings[0].len(), 1);
    }

    #[test]
    fn decodes_multipoint_one_ring_per_point() {
        let reader = build_source(vec![FeatureGeom {
            id: 1,
            bbox: [0.0, 0.0, 10.0, 10.0],
            geom: GeomKind::MultiPoint(vec![(1.0, 1.0), (5.0, 5.0), (9.0, 9.0)]),
        }]);
        let decoded = decode_source_geometry(&reader).unwrap();
        let f = &decoded.features[0];
        assert!(matches!(f.geom_type, GeomType::MultiPoint));
        assert_eq!(f.rings.len(), 3);
        assert!(f.rings.iter().all(|r| r.len() == 1));
    }

    #[test]
    fn decodes_multipolygon_flattens_rings() {
        let reader = build_source(vec![FeatureGeom {
            id: 1,
            bbox: [0.0, 0.0, 10.0, 10.0],
            geom: GeomKind::MultiPolygon(vec![
                vec![vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)]],
                vec![vec![(2.0, 2.0), (3.0, 2.0), (3.0, 3.0), (2.0, 2.0)]],
            ]),
        }]);
        let decoded = decode_source_geometry(&reader).unwrap();
        let f = &decoded.features[0];
        assert_eq!(f.rings.len(), 2);
    }

    fn dummy_decoded(bytes: usize) -> Arc<DecodedSourceGeometry> {
        // synthesise a decoded entry with the given byte cost. content is
        // immaterial — the cache only cares about the bytes field.
        Arc::new(DecodedSourceGeometry {
            features: Vec::new(),
            bytes,
        })
    }

    fn h(b: u8) -> ContentHash {
        ContentHash([b; 32])
    }

    #[test]
    fn cache_hit_bumps_recency() {
        let cache = DecodedGeometryCache::new(1024);
        cache.insert(h(1), dummy_decoded(100));
        cache.insert(h(2), dummy_decoded(100));
        // bump h(1) to most-recent
        let _ = cache.get(&h(1));
        cache.insert(h(3), dummy_decoded(900));
        // expect h(2) evicted, h(1) retained
        assert!(cache.get(&h(1)).is_some(), "h(1) should be retained as most-recent");
        assert!(cache.get(&h(2)).is_none(), "h(2) should have been evicted");
        assert!(cache.get(&h(3)).is_some());
    }

    #[test]
    fn cache_evicts_to_fit_max_bytes() {
        let cache = DecodedGeometryCache::new(250);
        cache.insert(h(1), dummy_decoded(100));
        cache.insert(h(2), dummy_decoded(100));
        cache.insert(h(3), dummy_decoded(100));
        // total would be 300; expect oldest (h(1)) evicted.
        assert!(cache.get(&h(1)).is_none());
        assert!(cache.get(&h(2)).is_some());
        assert!(cache.get(&h(3)).is_some());
        assert!(cache.current_bytes() <= 250);
    }

    #[test]
    fn oversized_single_entry_is_retained() {
        let cache = DecodedGeometryCache::new(50);
        cache.insert(h(1), dummy_decoded(500));
        // sole resident kept even though it exceeds the cap.
        assert!(cache.get(&h(1)).is_some());
    }

    #[test]
    fn reinsert_replaces_bytes_accounting() {
        let cache = DecodedGeometryCache::new(1024);
        cache.insert(h(1), dummy_decoded(100));
        cache.insert(h(1), dummy_decoded(50));
        assert_eq!(cache.current_bytes(), 50);
    }
}
