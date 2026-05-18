#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use bytes::Bytes;
use mars_source::GeometryEnvelope;
use mars_types::{Bbox, CrsCode};

use crate::plan::{BindingPlan, LevelPlan};
use crate::sidecar::encode_sidecar;

fn binding(id: &str) -> BindingPlan {
    BindingPlan {
        binding_id: BindingId::try_new(id).unwrap(),
        source_id: mars_config::SourceId::new("default"),
        source_table: id.to_string(),
        filter: None,
        geometry_field: "geom".into(),
        id_field: Some("id".into()),
        attributes: Vec::new(),
        native_crs: CrsCode::new("EPSG:25832"),
        levels: vec![LevelPlan {
            level: DecimationLevel::new(0),
            vertex_tolerance_m: 0.0,
            geometry_min_size_m: 0.0,
            label_min_priority: 0,
        }],
        page_size_target_bytes: 1024,
        sidecar_size_warn_bytes: u64::MAX,
        reconcile_every_cycles: 24,
        simplifier: mars_config::SimplifierKind::Naive,
        missing_page_policy: mars_config::MissingPagePolicy::Truncate,
        dsn: None,
    }
}

fn envelope(x: f64, y: f64) -> GeometryEnvelope {
    GeometryEnvelope {
        centroid: [x, y],
        bbox: Bbox::new(x, y, x, y),
    }
}

fn level(level: u8, ranges: Vec<(HilbertKey, HilbertKey)>) -> LevelMetadata {
    // synthesize identity-mapped page ids for the test fixtures; the
    // production code populates them from PageEntry.key.page_id.
    let table = ranges
        .into_iter()
        .enumerate()
        .map(|(i, (lo, hi))| (lo, hi, PageId::new(i as u64)))
        .collect::<Vec<_>>();
    LevelMetadata {
        level: DecimationLevel::new(level),
        vertex_tolerance_m: f64::from(level),
        geometry_min_size_m: 0.0,
        label_min_priority: 0,
        page_count: table.len() as u32,
        hilbert_range_table: table,
    }
}

fn binding_meta(id: &str, levels: Vec<LevelMetadata>) -> BindingMetadata {
    BindingMetadata {
        binding_id: BindingId::try_new(id).unwrap(),
        source_table: id.to_string(),
        native_crs: CrsCode::new("EPSG:25832"),
        feature_count_total: 0,
        combined_bbox: Bbox::new(0.0, 0.0, 100.0, 100.0),
        levels,
        page_membership_sidecar: None,
        cycles_since_reconcile: 0,
        last_reconcile_at: None,
    }
}

fn exact_ranges(points: &[[f64; 2]]) -> Vec<(HilbertKey, HilbertKey)> {
    let bbox = Bbox::new(0.0, 0.0, 100.0, 100.0);
    let mut keys: Vec<HilbertKey> = points.iter().map(|p| key_from_centroid(p[0], p[1], bbox)).collect();
    keys.sort_unstable();
    keys.dedup();
    keys.into_iter().map(|key| (key, key)).collect()
}

#[test]
fn ingest_marks_dirty_pages_and_truncates_binding() {
    let plan = BootstrapPlan {
        layers: Vec::new(),
        bindings: vec![binding("roads"), binding("buildings")],
        raster_layers: Vec::new(),
    };
    let ranges = exact_ranges(&[[10.0, 10.0], [20.0, 20.0], [30.0, 30.0], [40.0, 40.0], [50.0, 50.0]]);
    let bindings = HashMap::from([
        (
            BindingId::try_new("roads").unwrap(),
            binding_meta("roads", vec![level(0, ranges.clone()), level(1, ranges.clone())]),
        ),
        (
            BindingId::try_new("buildings").unwrap(),
            binding_meta("buildings", vec![level(0, ranges.clone())]),
        ),
    ]);

    // sidecar carries the prior-snapshot page membership for each
    // feature the cycle will revisit on its old side. roads fid 2/3
    // last lived at (40,40); fid 77 last lived at (30,30).
    let bbox = Bbox::new(0.0, 0.0, 100.0, 100.0);
    let key_30 = key_from_centroid(30.0, 30.0, bbox);
    let key_40 = key_from_centroid(40.0, 40.0, bbox);
    let mut sidecar_entries = vec![(2, key_40), (3, key_40), (77, key_30)];
    let sidecar_bytes: Bytes = encode_sidecar(&mut sidecar_entries).unwrap();
    let sidecar = SidecarReader::open(&sidecar_bytes).unwrap();
    let sidecars = HashMap::from([(BindingId::try_new("roads").unwrap(), sidecar)]);

    let mut cycle = IncrementalCycle::new(&plan, &sidecars, &bindings);
    cycle
        .ingest(ChangeEvent::Insert {
            collection: "roads".into(),
            feature_id: 1,
            new_envelope: envelope(10.0, 10.0),
        })
        .unwrap();
    cycle
        .ingest(ChangeEvent::Update {
            collection: "roads".into(),
            feature_id: 2,
            new_envelope: envelope(20.0, 20.0),
        })
        .unwrap();
    cycle
        .ingest(ChangeEvent::Update {
            collection: "roads".into(),
            feature_id: 77,
            new_envelope: envelope(50.0, 50.0),
        })
        .unwrap();
    cycle
        .ingest(ChangeEvent::Update {
            collection: "buildings".into(),
            feature_id: 999,
            new_envelope: envelope(10.0, 10.0),
        })
        .unwrap();
    cycle
        .ingest(ChangeEvent::Delete {
            collection: "roads".into(),
            feature_id: 3,
        })
        .unwrap();
    cycle
        .ingest(ChangeEvent::Delete {
            collection: "roads".into(),
            feature_id: 77,
        })
        .unwrap();
    cycle
        .ingest(ChangeEvent::Truncate {
            collection: "buildings".into(),
        })
        .unwrap();

    let dirty = cycle.finish();
    let roads = dirty.per_binding.get(&BindingId::try_new("roads").unwrap()).unwrap();
    assert!(!roads.truncated);
    assert_eq!(
        roads.per_level[&DecimationLevel::new(0)],
        BTreeSet::from_iter((0..5).map(PageId::new))
    );
    assert_eq!(
        roads.per_level[&DecimationLevel::new(1)],
        BTreeSet::from_iter((0..5).map(PageId::new))
    );

    // observed feature ids accumulated for non-truncated bindings.
    assert_eq!(roads.observed, BTreeSet::from_iter([1u64, 2, 3, 77]));

    let buildings = dirty
        .per_binding
        .get(&BindingId::try_new("buildings").unwrap())
        .unwrap();
    assert!(buildings.truncated);
    assert!(buildings.per_level.is_empty());
    // truncate clears observed: bootstrap path supersedes per-feature ids.
    assert!(buildings.observed.is_empty());
    assert_eq!(
        dirty.warnings,
        vec![IncrementalWarning::MissingOldGeometry {
            binding_id: BindingId::try_new("buildings").unwrap(),
            feature_id: 999,
        }]
    );
}

#[test]
fn duplicate_hilbert_key_marks_every_matching_page() {
    let key = HilbertKey::new(42);
    let page_ids = pages_for_key(&level(0, vec![(key, key), (key, key), (key, key)]), key);
    assert_eq!(
        BTreeSet::from_iter(page_ids),
        BTreeSet::from_iter([PageId::new(0), PageId::new(1), PageId::new(2)])
    );
}

#[test]
fn pages_for_key_returns_persisted_page_ids_not_table_index() {
    // simulate a manifest whose page_ids no longer match table position
    // (rebalance allocated fresh ids). pages_for_key must read the
    // persisted page_id, not synthesize from the array index.
    let key_lo = HilbertKey::new(10);
    let key_mid = HilbertKey::new(50);
    let key_hi = HilbertKey::new(90);
    let lvl = LevelMetadata {
        level: DecimationLevel::new(0),
        vertex_tolerance_m: 0.0,
        geometry_min_size_m: 0.0,
        label_min_priority: 0,
        page_count: 3,
        hilbert_range_table: vec![
            (key_lo, key_lo, PageId::new(7)),
            (key_mid, key_mid, PageId::new(42)),
            (key_hi, key_hi, PageId::new(99)),
        ],
    };
    assert_eq!(pages_for_key(&lvl, key_mid), vec![PageId::new(42)]);
    assert_eq!(pages_for_key(&lvl, key_lo), vec![PageId::new(7)]);
    assert_eq!(pages_for_key(&lvl, key_hi), vec![PageId::new(99)]);
    assert!(pages_for_key(&lvl, HilbertKey::new(11)).is_empty());
}
