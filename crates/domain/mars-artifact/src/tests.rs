use bytes::Bytes;
use mars_types::{Bbox, ContentHash};
use proptest::prelude::*;

use crate::{
    ArtifactError, ArtifactKind, ArtifactReader, ArtifactWriter, FORMAT_VERSION, MAGIC, SectionKind, SourceRef,
    compute_content_hash, decode_class_assignment, decode_geometry_at_slots, decode_geometry_payload,
    decode_geometry_payload_filtered, decode_one_geom, decode_style_refs, encode_geometry_payload, iter_feature_index,
    visit_one_geom,
};
use crate::{Coord, FeatureGeom, GeomKind, GeomVisitor};

const MM_TOL: f64 = 1.0 / 1000.0;

fn coord_close(a: Coord, b: Coord) -> bool {
    (a.0 - b.0).abs() <= MM_TOL && (a.1 - b.1).abs() <= MM_TOL
}

fn ring_close(a: &[Coord], b: &[Coord]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| coord_close(*x, *y))
}

fn geom_close(a: &GeomKind, b: &GeomKind) -> bool {
    match (a, b) {
        (GeomKind::Point(p), GeomKind::Point(q)) => coord_close(*p, *q),
        (GeomKind::LineString(p), GeomKind::LineString(q)) => ring_close(p, q),
        (GeomKind::Polygon(p), GeomKind::Polygon(q)) => {
            p.len() == q.len() && p.iter().zip(q).all(|(r, s)| ring_close(r, s))
        }
        (GeomKind::MultiPoint(p), GeomKind::MultiPoint(q)) => ring_close(p, q),
        (GeomKind::MultiLineString(p), GeomKind::MultiLineString(q)) => {
            p.len() == q.len() && p.iter().zip(q).all(|(r, s)| ring_close(r, s))
        }
        (GeomKind::MultiPolygon(p), GeomKind::MultiPolygon(q)) => {
            p.len() == q.len()
                && p.iter()
                    .zip(q)
                    .all(|(r, s)| r.len() == s.len() && r.iter().zip(s).all(|(rr, ss)| ring_close(rr, ss)))
        }
        _ => false,
    }
}

fn coord_strategy() -> impl Strategy<Value = Coord> {
    (-1_000_000.0_f64..1_000_000.0, -1_000_000.0_f64..1_000_000.0)
}

fn ring_strategy() -> impl Strategy<Value = Vec<Coord>> {
    prop::collection::vec(coord_strategy(), 0..8)
}

fn geom_strategy() -> impl Strategy<Value = GeomKind> {
    prop_oneof![
        coord_strategy().prop_map(GeomKind::Point),
        ring_strategy().prop_map(GeomKind::LineString),
        prop::collection::vec(ring_strategy(), 0..4).prop_map(GeomKind::Polygon),
        prop::collection::vec(coord_strategy(), 0..8).prop_map(GeomKind::MultiPoint),
        prop::collection::vec(ring_strategy(), 0..4).prop_map(GeomKind::MultiLineString),
        prop::collection::vec(prop::collection::vec(ring_strategy(), 0..3), 0..3).prop_map(GeomKind::MultiPolygon),
    ]
}

prop_compose! {
    fn feature_strategy()(g in geom_strategy()) -> GeomKind { g }
}

prop_compose! {
    fn features_strategy()(geoms in prop::collection::vec(geom_strategy(), 0..16)) -> Vec<FeatureGeom> {
        // ids must be strictly ascending and unique per encoder contract.
        let mut out = Vec::with_capacity(geoms.len());
        for (i, g) in geoms.into_iter().enumerate() {
            out.push(FeatureGeom { id: i as u64, bbox: [0.0; 4], geom: g });
        }
        out
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn geometry_payload_roundtrip(features in features_strategy()) {
        let bytes = encode_geometry_payload(&features).unwrap();
        let back = decode_geometry_payload(&bytes).unwrap();
        prop_assert_eq!(features.len(), back.len());
        for (a, b) in features.iter().zip(&back) {
            prop_assert_eq!(a.id, b.id);
            prop_assert_eq!(a.bbox, b.bbox);
            prop_assert!(geom_close(&a.geom, &b.geom));
        }
    }

    #[test]
    fn geometry_payload_deterministic(features in features_strategy()) {
        let a = encode_geometry_payload(&features).unwrap();
        let b = encode_geometry_payload(&features).unwrap();
        prop_assert_eq!(a, b);
    }

    /// filtered decode must match decode-then-filter on equivalent predicates.
    #[test]
    fn geometry_payload_filter_parity(features in features_strategy()) {
        let bytes = encode_geometry_payload(&features).unwrap();

        // pred selects every other id: cheap, deterministic, mixes pass/skip.
        let pred = |id: u64, _bbox: [f32; 4]| id.is_multiple_of(2);

        let filtered = decode_geometry_payload_filtered(&bytes, pred).unwrap();
        let full: Vec<FeatureGeom> = decode_geometry_payload(&bytes)
            .unwrap()
            .into_iter()
            .filter(|f| pred(f.id, f.bbox))
            .collect();

        prop_assert_eq!(filtered.len(), full.len());
        for (a, b) in filtered.iter().zip(&full) {
            prop_assert_eq!(a.id, b.id);
            prop_assert_eq!(a.bbox, b.bbox);
            prop_assert!(geom_close(&a.geom, &b.geom));
        }
    }

    /// `visit_one_geom` must emit coords in the same document order as a
    /// flatten of `decode_one_geom` for every geometry kind.
    #[test]
    fn visitor_coords_match_decode_one_geom(features in features_strategy()) {
        let bytes = encode_geometry_payload(&features).unwrap();
        let iter = iter_feature_index(&bytes).unwrap();
        let coord_area = iter.coord_area();
        for entry in iter {
            let entry = entry.unwrap();
            let decoded = decode_one_geom(coord_area, &entry).unwrap();
            let mut visitor = PointCollector::default();
            visit_one_geom(coord_area, &entry, &mut visitor).unwrap();
            let want = flatten_geom(&decoded);
            prop_assert_eq!(want.len(), visitor.coords.len());
            for (a, b) in want.iter().zip(&visitor.coords) {
                prop_assert!(coord_close(*a, *b));
            }
        }
    }
}

#[derive(Default)]
struct PointCollector {
    coords: Vec<Coord>,
}

impl GeomVisitor for PointCollector {
    fn point(&mut self, x: f64, y: f64) {
        self.coords.push((x, y));
    }
    fn begin_ring(&mut self) {}
    fn end_ring(&mut self) {}
    fn begin_part(&mut self) {}
    fn end_part(&mut self) {}
}

fn flatten_geom(g: &GeomKind) -> Vec<Coord> {
    match g {
        GeomKind::Point(p) => vec![*p],
        GeomKind::LineString(verts) => verts.clone(),
        GeomKind::Polygon(rings) => rings.iter().flatten().copied().collect(),
        GeomKind::MultiPoint(pts) => pts.clone(),
        GeomKind::MultiLineString(parts) => parts.iter().flatten().copied().collect(),
        GeomKind::MultiPolygon(parts) => parts.iter().flatten().flatten().copied().collect(),
    }
}

#[derive(Debug, Default, PartialEq)]
struct EventTrace {
    events: Vec<Event>,
}

#[derive(Debug, PartialEq)]
enum Event {
    Point,
    BeginRing,
    EndRing,
    BeginPart,
    EndPart,
}

impl GeomVisitor for EventTrace {
    fn point(&mut self, _x: f64, _y: f64) {
        self.events.push(Event::Point);
    }
    fn begin_ring(&mut self) {
        self.events.push(Event::BeginRing);
    }
    fn end_ring(&mut self) {
        self.events.push(Event::EndRing);
    }
    fn begin_part(&mut self) {
        self.events.push(Event::BeginPart);
    }
    fn end_part(&mut self) {
        self.events.push(Event::EndPart);
    }
}

#[test]
fn visitor_event_shape_per_geom_kind() {
    use Event::*;
    let cases: &[(GeomKind, &[Event])] = &[
        (GeomKind::Point((1.0, 2.0)), &[BeginPart, Point, EndPart]),
        (
            GeomKind::LineString(vec![(0.0, 0.0), (1.0, 1.0)]),
            &[BeginPart, BeginRing, Point, Point, EndRing, EndPart],
        ),
        (
            GeomKind::Polygon(vec![vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)]]),
            &[BeginPart, BeginRing, Point, Point, Point, Point, EndRing, EndPart],
        ),
        (
            GeomKind::MultiPoint(vec![(0.0, 0.0), (1.0, 1.0)]),
            &[BeginPart, Point, EndPart, BeginPart, Point, EndPart],
        ),
    ];

    for (g, want) in cases {
        let features = vec![FeatureGeom {
            id: 1,
            bbox: [0.0; 4],
            geom: g.clone(),
        }];
        let bytes = encode_geometry_payload(&features).unwrap();
        let iter = iter_feature_index(&bytes).unwrap();
        let coord_area = iter.coord_area();
        let entry = iter.into_iter().next().unwrap().unwrap();
        let mut trace = EventTrace::default();
        visit_one_geom(coord_area, &entry, &mut trace).unwrap();
        assert_eq!(&trace.events, want, "event mismatch for {g:?}");
    }
}

#[test]
fn empty_geometries_roundtrip() {
    let features = vec![
        FeatureGeom {
            id: 0,
            bbox: [0.0; 4],
            geom: GeomKind::LineString(vec![]),
        },
        FeatureGeom {
            id: 1,
            bbox: [0.0; 4],
            geom: GeomKind::Polygon(vec![]),
        },
        FeatureGeom {
            id: 2,
            bbox: [0.0; 4],
            geom: GeomKind::MultiPolygon(vec![]),
        },
    ];
    let bytes = encode_geometry_payload(&features).unwrap();
    let back = decode_geometry_payload(&bytes).unwrap();
    assert_eq!(features.len(), back.len());
    for (a, b) in features.iter().zip(&back) {
        assert_eq!(a.id, b.id);
        assert!(geom_close(&a.geom, &b.geom));
    }
}

#[test]
fn class_assignment_roundtrip() {
    let items = vec![(1u64, 0u16), (5, 2), (42, 7)];
    let bytes = crate::encode_class_assignment(&items).unwrap();
    let back = decode_class_assignment(&bytes).unwrap();
    assert_eq!(items, back);
}

#[test]
fn style_refs_roundtrip() {
    let refs = vec!["bygning_normal".to_owned(), "vejmidte_motorvej".to_owned()];
    let bytes = crate::encode_style_refs(&refs);
    let back = decode_style_refs(&bytes).unwrap();
    assert_eq!(refs, back);
}

fn build_simple_artifact() -> Bytes {
    let features = vec![FeatureGeom {
        id: 1,
        bbox: [0.0, 0.0, 10.0, 10.0],
        geom: GeomKind::LineString(vec![(0.0, 0.0), (10.0, 10.0)]),
    }];
    let mut w = ArtifactWriter::new(ArtifactKind::Source);
    w.add_geometry_payload(features)
        .set_bbox(Bbox::new(0.0, 0.0, 10.0, 10.0))
        .set_feature_count(1);
    w.finish().unwrap()
}

#[test]
fn writer_reader_roundtrip() {
    let bytes = build_simple_artifact();
    let r = ArtifactReader::open(bytes).unwrap();
    assert_eq!(r.kind(), ArtifactKind::Source);
    assert_eq!(r.feature_count(), 1);
    assert_eq!(r.bbox(), Bbox::new(0.0, 0.0, 10.0, 10.0));
    let geom = r.section(SectionKind::GeometryPayload).unwrap();
    let back = decode_geometry_payload(&geom).unwrap();
    assert_eq!(back.len(), 1);
}

#[test]
fn artifact_deterministic() {
    let a = build_simple_artifact();
    let b = build_simple_artifact();
    assert_eq!(a, b);
    assert_eq!(compute_content_hash(&a).0, compute_content_hash(&b).0);
}

#[test]
fn layer_artifact_with_source_ref() {
    let mut w = ArtifactWriter::new(ArtifactKind::Layer);
    w.add_class_assignment(&[(1, 0), (2, 1)])
        .add_style_refs(&["a".to_owned(), "b".to_owned()])
        .set_bbox(Bbox::new(0.0, 0.0, 1.0, 1.0))
        .set_feature_count(2)
        .set_source_ref(SourceRef {
            collection: "bygning".into(),
            band: "hi".into(),
            cell_x: 3,
            cell_y: 4,
            content_hash: ContentHash([7u8; 32]),
        });
    let bytes = w.finish().unwrap();
    let r = ArtifactReader::open(bytes).unwrap();
    assert_eq!(r.kind(), ArtifactKind::Layer);
    let s = r.source_ref().unwrap();
    assert_eq!(s.collection, "bygning");
    assert_eq!(s.band, "hi");
    assert_eq!(s.cell_x, 3);
    assert_eq!(s.cell_y, 4);
    assert_eq!(s.content_hash.0, [7u8; 32]);
    let ca = r.section(SectionKind::ClassAssignment).unwrap();
    assert_eq!(decode_class_assignment(&ca).unwrap(), vec![(1, 0), (2, 1)]);
    let sr = r.section(SectionKind::StyleRefs).unwrap();
    assert_eq!(decode_style_refs(&sr).unwrap(), vec!["a".to_owned(), "b".to_owned()]);
}

#[test]
fn spatial_index_typed_roundtrip_through_envelope() {
    // build a small index, embed it in a source artifact via the typed
    // writer helper, reopen, and exercise the query path.
    let mut b = crate::SpatialIndexBuilder::new(crate::DEFAULT_NODE_SIZE).unwrap();
    b.add(0, [0.0, 0.0, 1.0, 1.0])
        .add(1, [10.0, 10.0, 11.0, 11.0])
        .add(2, [20.0, 20.0, 21.0, 21.0]);
    let payload = b.finish().unwrap();

    let mut w = ArtifactWriter::new(ArtifactKind::Source);
    w.add_spatial_index(payload.clone())
        .set_bbox(Bbox::new(0.0, 0.0, 21.0, 21.0))
        .set_feature_count(0);
    let bytes = w.finish().unwrap();

    let r = ArtifactReader::open(bytes).unwrap();
    let back = r.section(SectionKind::SpatialIndex).unwrap();
    assert_eq!(back, payload);

    let idx = crate::SpatialIndex::open(back).unwrap();
    assert_eq!(idx.len(), 3);
    let mut hits = Vec::new();
    idx.query([9.5, 9.5, 11.5, 11.5], &mut hits);
    assert_eq!(hits, vec![1]);
}

#[test]
fn rejects_truncated_at_every_boundary() {
    let bytes = build_simple_artifact();
    // header truncations
    for cut in 0..=15 {
        let r = ArtifactReader::open(bytes.slice(..cut.min(bytes.len())));
        assert!(r.is_err(), "expected error at cut {cut}");
    }
    // footer / trailer truncations: drop tail bytes one at a time up to 16
    for cut in 1..=16 {
        let n = bytes.len() - cut;
        let r = ArtifactReader::open(bytes.slice(..n));
        assert!(r.is_err(), "expected error at tail-cut {cut}");
    }
    // section payload truncation: cut somewhere in the middle
    let mid = bytes.len() / 2;
    let r = ArtifactReader::open(bytes.slice(..mid));
    assert!(r.is_err());
}

#[test]
fn rejects_bad_magic() {
    let mut buf = vec![0u8; 32];
    buf[..8].copy_from_slice(b"NOTMARS!");
    let r = ArtifactReader::open(Bytes::from(buf));
    assert!(matches!(r, Err(ArtifactError::BadMagic)));
}

#[test]
fn rejects_unsupported_version() {
    let mut buf = build_simple_artifact().to_vec();
    let bogus: u32 = FORMAT_VERSION + 1;
    buf[8..12].copy_from_slice(&bogus.to_le_bytes());
    // need a fresh trailing magic (it is already there); just open
    let r = ArtifactReader::open(Bytes::from(buf));
    assert!(matches!(r, Err(ArtifactError::UnsupportedVersion(_))));
}

#[test]
fn writer_rejects_duplicate_section_kinds() {
    let payload = encode_geometry_payload(&[]).unwrap();
    let mut w = ArtifactWriter::new(ArtifactKind::Source);
    w.add_section(SectionKind::GeometryPayload, payload.clone())
        .add_section(SectionKind::GeometryPayload, payload)
        .set_bbox(Bbox::new(0.0, 0.0, 1.0, 1.0))
        .set_feature_count(0);
    let err = w.finish().unwrap_err();
    assert!(matches!(err, ArtifactError::DuplicateSection(_)), "got {err:?}");
}

#[test]
fn rejects_compressed_section_flag() {
    // synthesize an artifact whose lone section header has FLAG_COMPRESSED set,
    // by patching the writer's output.
    let mut buf = build_simple_artifact().to_vec();
    // first section header lives just after the 16-byte file header
    let hdr_off = 16;
    // flags is bytes [hdr_off+2 .. hdr_off+4]
    buf[hdr_off + 2] = 0x01;
    let r = ArtifactReader::open(Bytes::from(buf)).unwrap();
    let err = r.section(SectionKind::GeometryPayload).unwrap_err();
    assert!(matches!(err, ArtifactError::CompressedNotSupported));
}

#[test]
fn magic_constant_matches_spec() {
    assert_eq!(MAGIC, b"MARS\0\0\0\0");
}

#[test]
fn writer_requires_bbox() {
    let mut w = ArtifactWriter::new(ArtifactKind::Source);
    w.set_feature_count(0);
    assert!(matches!(w.finish(), Err(ArtifactError::InvalidWriterState(_))));
}

#[test]
fn writer_rejects_source_ref_on_source_kind() {
    let mut w = ArtifactWriter::new(ArtifactKind::Source);
    w.set_bbox(Bbox::new(0.0, 0.0, 1.0, 1.0)).set_source_ref(SourceRef {
        collection: "c".into(),
        band: "b".into(),
        cell_x: 0,
        cell_y: 0,
        content_hash: ContentHash([0u8; 32]),
    });
    assert!(matches!(w.finish(), Err(ArtifactError::InvalidWriterState(_))));
}

#[test]
fn writer_validates_feature_count_against_payload() {
    let features = vec![FeatureGeom {
        id: 1,
        bbox: [0.0; 4],
        geom: GeomKind::Point((0.0, 0.0)),
    }];
    let mut w = ArtifactWriter::new(ArtifactKind::Source);
    w.add_geometry_payload(features)
        .set_bbox(Bbox::new(0.0, 0.0, 1.0, 1.0))
        .set_feature_count(99);
    assert!(matches!(w.finish(), Err(ArtifactError::InvalidWriterState(_))));
}

#[test]
fn decode_geometry_at_slots_returns_only_requested() {
    let features = vec![
        FeatureGeom {
            id: 10,
            bbox: [0.0, 0.0, 1.0, 1.0],
            geom: GeomKind::Point((0.5, 0.5)),
        },
        FeatureGeom {
            id: 20,
            bbox: [1.0, 1.0, 2.0, 2.0],
            geom: GeomKind::Point((1.5, 1.5)),
        },
        FeatureGeom {
            id: 30,
            bbox: [2.0, 2.0, 3.0, 3.0],
            geom: GeomKind::Point((2.5, 2.5)),
        },
    ];
    let bytes = encode_geometry_payload(&features).unwrap();
    let got = decode_geometry_at_slots(&bytes, &[2, 0]).unwrap();
    assert_eq!(got.len(), 2);
    let ids: Vec<u64> = got.iter().map(|f| f.id).collect();
    assert!(ids.contains(&10));
    assert!(ids.contains(&30));
    assert!(!ids.contains(&20));
}

#[test]
fn decode_geometry_at_slots_dedupes_input() {
    let features = vec![FeatureGeom {
        id: 7,
        bbox: [0.0, 0.0, 1.0, 1.0],
        geom: GeomKind::Point((0.0, 0.0)),
    }];
    let bytes = encode_geometry_payload(&features).unwrap();
    let got = decode_geometry_at_slots(&bytes, &[0, 0, 0]).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].id, 7);
}

#[test]
fn decode_geometry_at_slots_silently_drops_oob() {
    let features = vec![FeatureGeom {
        id: 1,
        bbox: [0.0, 0.0, 1.0, 1.0],
        geom: GeomKind::Point((0.0, 0.0)),
    }];
    let bytes = encode_geometry_payload(&features).unwrap();
    let got = decode_geometry_at_slots(&bytes, &[42]).unwrap();
    assert!(got.is_empty());
}

#[test]
fn writer_derives_feature_count_from_staged_payload() {
    let features = vec![
        FeatureGeom {
            id: 1,
            bbox: [0.0; 4],
            geom: GeomKind::Point((0.0, 0.0)),
        },
        FeatureGeom {
            id: 2,
            bbox: [0.0; 4],
            geom: GeomKind::Point((1.0, 1.0)),
        },
    ];
    let mut w = ArtifactWriter::new(ArtifactKind::Source);
    w.add_geometry_payload(features).set_bbox(Bbox::new(0.0, 0.0, 1.0, 1.0));
    let bytes = w.finish().unwrap();
    let reader = ArtifactReader::open(bytes).unwrap();
    assert_eq!(reader.feature_count(), 2);
}

#[test]
fn writer_rejects_geometry_without_feature_count() {
    // raw geometry section bypasses the staging path; feature_count must
    // then be set explicitly or the footer would silently say zero.
    let mut w = ArtifactWriter::new(ArtifactKind::Source);
    w.add_section(SectionKind::GeometryPayload, Bytes::from_static(&[0u8; 4]))
        .set_bbox(Bbox::new(0.0, 0.0, 1.0, 1.0));
    assert!(matches!(w.finish(), Err(ArtifactError::InvalidWriterState(_))));
}

#[test]
fn class_assignment_rejects_unsorted() {
    // hand-build: count=2, ids 5,1 (decreasing)
    let mut buf = Vec::new();
    buf.extend_from_slice(&2u32.to_le_bytes());
    buf.extend_from_slice(&5u64.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&1u64.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    assert!(matches!(
        decode_class_assignment(&buf),
        Err(ArtifactError::Malformed(_))
    ));
}

#[test]
fn class_assignment_rejects_duplicate() {
    let mut buf = Vec::new();
    buf.extend_from_slice(&2u32.to_le_bytes());
    buf.extend_from_slice(&5u64.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&5u64.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    assert!(matches!(
        decode_class_assignment(&buf),
        Err(ArtifactError::Malformed(_))
    ));
}

#[test]
fn class_assignment_rejects_trailing_bytes() {
    let mut buf = crate::encode_class_assignment(&[(1u64, 0u16)]).unwrap().to_vec();
    buf.push(0);
    let err = decode_class_assignment(&buf).unwrap_err();
    assert!(matches!(err, ArtifactError::Malformed(_)));
}

#[test]
fn style_refs_rejects_huge_count() {
    // declare u32::MAX entries in a 4-byte buffer; must not OOM
    let mut buf = Vec::new();
    buf.extend_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(decode_style_refs(&buf), Err(ArtifactError::Truncated)));
}

#[test]
fn geometry_rejects_unsorted_features() {
    let features = vec![
        FeatureGeom {
            id: 5,
            bbox: [0.0; 4],
            geom: GeomKind::Point((0.0, 0.0)),
        },
        FeatureGeom {
            id: 1,
            bbox: [0.0; 4],
            geom: GeomKind::Point((1.0, 1.0)),
        },
    ];
    assert!(matches!(
        encode_geometry_payload(&features),
        Err(ArtifactError::UnsortedFeatures)
    ));
}

#[test]
fn geometry_rejects_non_finite_coord() {
    let features = vec![FeatureGeom {
        id: 0,
        bbox: [0.0; 4],
        geom: GeomKind::Point((f64::NAN, 0.0)),
    }];
    assert!(matches!(
        encode_geometry_payload(&features),
        Err(ArtifactError::CoordOutOfRange(_))
    ));
}

#[test]
fn geometry_rejects_oversize_coord() {
    let features = vec![FeatureGeom {
        id: 0,
        bbox: [0.0; 4],
        geom: GeomKind::Point((1e20, 0.0)),
    }];
    assert!(matches!(
        encode_geometry_payload(&features),
        Err(ArtifactError::CoordOutOfRange(_))
    ));
}

fn malicious_payload(geom_type: u8, coord_bytes: &[u8]) -> Vec<u8> {
    use crate::geometry::FEATURE_INDEX_ENTRY_LEN;
    let mut out = Vec::new();
    out.extend_from_slice(&1u32.to_le_bytes()); // count = 1
    out.extend_from_slice(&0u64.to_le_bytes()); // id
    for _ in 0..4 {
        out.extend_from_slice(&0f32.to_le_bytes());
    }
    out.push(geom_type);
    out.extend_from_slice(&0u32.to_le_bytes()); // coord offset
    out.extend_from_slice(&(coord_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(coord_bytes);
    // pad to expected header_len so decode_geometry_payload doesn't truncate early
    let header_len = 4 + FEATURE_INDEX_ENTRY_LEN;
    while out.len() < header_len {
        out.push(0);
    }
    out
}

#[test]
fn geometry_rejects_huge_ring_count() {
    use crate::geometry::{GT_LINESTRING, MAX_GEOM_COORDS};
    use crate::varint::{write_ivarint, write_uvarint};
    let mut coords = Vec::new();
    write_uvarint(&mut coords, (MAX_GEOM_COORDS + 1) as u64);
    write_ivarint(&mut coords, 0);
    write_ivarint(&mut coords, 0);
    let payload = malicious_payload(GT_LINESTRING, &coords);
    assert!(matches!(
        decode_geometry_payload(&payload),
        Err(ArtifactError::Malformed(_))
    ));
}

#[test]
fn geometry_rejects_huge_polygon_ring_count() {
    use crate::geometry::{GT_POLYGON, MAX_GEOM_PARTS};
    use crate::varint::write_uvarint;
    let mut coords = Vec::new();
    write_uvarint(&mut coords, (MAX_GEOM_PARTS + 1) as u64);
    let payload = malicious_payload(GT_POLYGON, &coords);
    assert!(matches!(
        decode_geometry_payload(&payload),
        Err(ArtifactError::Malformed(_))
    ));
}

#[test]
fn geometry_rejects_huge_multipoint_count() {
    use crate::geometry::{GT_MULTIPOINT, MAX_GEOM_COORDS};
    use crate::varint::write_uvarint;
    let mut coords = Vec::new();
    write_uvarint(&mut coords, (MAX_GEOM_COORDS + 1) as u64);
    let payload = malicious_payload(GT_MULTIPOINT, &coords);
    assert!(matches!(
        decode_geometry_payload(&payload),
        Err(ArtifactError::Malformed(_))
    ));
}

#[test]
fn geometry_rejects_huge_multilinestring_part_count() {
    use crate::geometry::{GT_MULTILINESTRING, MAX_GEOM_PARTS};
    use crate::varint::write_uvarint;
    let mut coords = Vec::new();
    write_uvarint(&mut coords, (MAX_GEOM_PARTS + 1) as u64);
    let payload = malicious_payload(GT_MULTILINESTRING, &coords);
    assert!(matches!(
        decode_geometry_payload(&payload),
        Err(ArtifactError::Malformed(_))
    ));
}

#[test]
fn geometry_rejects_huge_multipolygon_count() {
    use crate::geometry::{GT_MULTIPOLYGON, MAX_GEOM_PARTS};
    use crate::varint::write_uvarint;
    let mut coords = Vec::new();
    write_uvarint(&mut coords, (MAX_GEOM_PARTS + 1) as u64);
    let payload = malicious_payload(GT_MULTIPOLYGON, &coords);
    assert!(matches!(
        decode_geometry_payload(&payload),
        Err(ArtifactError::Malformed(_))
    ));
}

#[test]
fn geometry_rejects_delta_overflow() {
    use crate::geometry::GT_LINESTRING;
    use crate::varint::{write_ivarint, write_uvarint};
    let mut coords = Vec::new();
    write_uvarint(&mut coords, 2u64); // 2 points
    write_ivarint(&mut coords, 1); // first x = 1
    write_ivarint(&mut coords, 0); // first y
    write_ivarint(&mut coords, i64::MAX); // 1 + i64::MAX overflows
    write_ivarint(&mut coords, 0);
    let payload = malicious_payload(GT_LINESTRING, &coords);
    assert!(matches!(
        decode_geometry_payload(&payload),
        Err(ArtifactError::Malformed(_))
    ));
}

// ---- spatial index ----------------------------------------------------------

mod spatial_index_tests {
    use super::*;
    use crate::{DEFAULT_NODE_SIZE, SpatialIndex, SpatialIndexBuilder};

    fn build(items: &[(u32, [f32; 4])], node_size: u16) -> Bytes {
        let mut b = SpatialIndexBuilder::new(node_size).unwrap();
        for &(idx, bb) in items {
            b.add(idx, bb);
        }
        b.finish().unwrap()
    }

    fn brute_force(items: &[(u32, [f32; 4])], q: [f32; 4]) -> Vec<u32> {
        let mut out = Vec::new();
        for &(idx, bb) in items {
            if bb[0] <= q[2] && bb[1] <= q[3] && bb[2] >= q[0] && bb[3] >= q[1] {
                out.push(idx);
            }
        }
        out.sort_unstable();
        out
    }

    fn run_query(idx: &SpatialIndex, q: [f32; 4]) -> Vec<u32> {
        let mut hits = Vec::new();
        idx.query(q, &mut hits);
        hits.sort_unstable();
        hits
    }

    #[test]
    fn empty_builds_and_queries() {
        let bytes = build(&[], DEFAULT_NODE_SIZE);
        let idx = SpatialIndex::open(bytes).unwrap();
        assert!(idx.is_empty());
        assert_eq!(idx.len(), 0);
        let mut out = Vec::new();
        idx.query([-1.0, -1.0, 1.0, 1.0], &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn single_item_roundtrips() {
        let items = vec![(42, [0.0, 0.0, 1.0, 1.0])];
        let idx = SpatialIndex::open(build(&items, DEFAULT_NODE_SIZE)).unwrap();
        assert_eq!(idx.len(), 1);
        assert_eq!(run_query(&idx, [0.5, 0.5, 0.5, 0.5]), vec![42]);
        assert_eq!(run_query(&idx, [10.0, 10.0, 11.0, 11.0]), vec![]);
    }

    #[test]
    fn disjoint_query_returns_nothing() {
        let items: Vec<_> = (0..50)
            .map(|i| (i as u32, [i as f32, 0.0, i as f32 + 0.5, 1.0]))
            .collect();
        let idx = SpatialIndex::open(build(&items, DEFAULT_NODE_SIZE)).unwrap();
        assert_eq!(run_query(&idx, [-1000.0, -1000.0, -999.0, -999.0]), vec![]);
    }

    #[test]
    fn degenerate_collinear_inputs_build() {
        // all items share y; width > 0, height == 0
        let items: Vec<_> = (0..32)
            .map(|i| (i as u32, [i as f32, 0.0, i as f32 + 0.5, 0.0]))
            .collect();
        let idx = SpatialIndex::open(build(&items, DEFAULT_NODE_SIZE)).unwrap();
        let q = [5.0, -1.0, 8.0, 1.0];
        assert_eq!(run_query(&idx, q), brute_force(&items, q));
    }

    #[test]
    fn degenerate_point_cloud_at_origin_builds() {
        // all items are the same point: width == height == 0
        let items: Vec<_> = (0..40).map(|i| (i as u32, [3.0, 4.0, 3.0, 4.0])).collect();
        let idx = SpatialIndex::open(build(&items, DEFAULT_NODE_SIZE)).unwrap();
        let q = [3.0, 4.0, 3.0, 4.0];
        let mut hits = run_query(&idx, q);
        hits.sort_unstable();
        let mut want: Vec<u32> = (0..40).collect();
        want.sort_unstable();
        assert_eq!(hits, want);
    }

    #[test]
    fn varies_node_size_yields_same_result() {
        let items: Vec<_> = (0..200u32)
            .map(|i| {
                let x = (i % 20) as f32 * 5.0;
                let y = (i / 20) as f32 * 5.0;
                (i, [x, y, x + 1.0, y + 1.0])
            })
            .collect();
        let q = [10.0, 10.0, 30.0, 30.0];
        let want = brute_force(&items, q);
        for ns in [2u16, 4, 8, 16, 32, 64] {
            let idx = SpatialIndex::open(build(&items, ns)).unwrap();
            assert_eq!(run_query(&idx, q), want, "node_size {ns}");
        }
    }

    #[test]
    fn build_is_deterministic_for_same_input() {
        let items: Vec<_> = (0..100u32)
            .map(|i| {
                let x = ((i * 31) % 97) as f32;
                let y = ((i * 17) % 53) as f32;
                (i, [x, y, x + 0.5, y + 0.5])
            })
            .collect();
        let a = build(&items, DEFAULT_NODE_SIZE);
        let b = build(&items, DEFAULT_NODE_SIZE);
        assert_eq!(a, b);
    }

    #[test]
    fn query_visit_matches_query() {
        let items: Vec<_> = (0..100u32).map(|i| (i, [i as f32, 0.0, i as f32 + 1.0, 1.0])).collect();
        let idx = SpatialIndex::open(build(&items, DEFAULT_NODE_SIZE)).unwrap();
        let q = [10.0, -1.0, 25.0, 2.0];
        let mut a: Vec<u32> = Vec::new();
        idx.query(q, &mut a);
        let mut b: Vec<u32> = Vec::new();
        idx.query_visit(q, |i| b.push(i));
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn query_matches_brute_force(
            items in prop::collection::vec(
                ((-1000.0f32..1000.0), (-1000.0f32..1000.0), (0.0f32..50.0), (0.0f32..50.0)),
                0..200
            ),
            q in (
                (-1500.0f32..1500.0), (-1500.0f32..1500.0),
                (0.0f32..200.0), (0.0f32..200.0),
            ),
        ) {
            let items: Vec<(u32, [f32; 4])> = items
                .into_iter()
                .enumerate()
                .map(|(i, (x, y, w, h))| (i as u32, [x, y, x + w, y + h]))
                .collect();
            let qbb = [q.0, q.1, q.0 + q.2, q.1 + q.3];

            let bytes = build(&items, DEFAULT_NODE_SIZE);
            let idx = SpatialIndex::open(bytes).unwrap();
            let got = run_query(&idx, qbb);
            let want = brute_force(&items, qbb);
            prop_assert_eq!(got, want);
        }

        #[test]
        fn determinism_proptest(
            items in prop::collection::vec(
                ((-100.0f32..100.0), (-100.0f32..100.0)),
                0..64
            ),
        ) {
            let items: Vec<(u32, [f32; 4])> = items
                .into_iter()
                .enumerate()
                .map(|(i, (x, y))| (i as u32, [x, y, x + 0.5, y + 0.5]))
                .collect();
            let a = build(&items, DEFAULT_NODE_SIZE);
            let b = build(&items, DEFAULT_NODE_SIZE);
            prop_assert_eq!(a, b);
        }

        #[test]
        fn node_size_invariant(
            items in prop::collection::vec(
                ((-50.0f32..50.0), (-50.0f32..50.0)),
                4..80
            ),
            qx in (-60.0f32..60.0),
            qy in (-60.0f32..60.0),
            qw in (0.0f32..40.0),
            qh in (0.0f32..40.0),
        ) {
            let items: Vec<(u32, [f32; 4])> = items
                .into_iter()
                .enumerate()
                .map(|(i, (x, y))| (i as u32, [x, y, x + 0.5, y + 0.5]))
                .collect();
            let q = [qx, qy, qx + qw, qy + qh];
            let mut prev: Option<Vec<u32>> = None;
            for ns in [2u16, 4, 8, 16, 32] {
                let idx = SpatialIndex::open(build(&items, ns)).unwrap();
                let got = run_query(&idx, q);
                if let Some(p) = &prev {
                    prop_assert_eq!(p, &got);
                }
                prev = Some(got);
            }
        }
    }

    // ---- malformed-input rejection. all paths must Err, never panic. ---------

    fn good_bytes() -> Vec<u8> {
        let items: Vec<_> = (0..64u32).map(|i| (i, [i as f32, 0.0, i as f32 + 1.0, 1.0])).collect();
        build(&items, 4).to_vec()
    }

    #[test]
    fn rejects_truncated_header() {
        let good = good_bytes();
        for cut in 0..36 {
            let r = SpatialIndex::open(Bytes::copy_from_slice(&good[..cut]));
            assert!(r.is_err(), "expected error at cut {cut}");
        }
    }

    #[test]
    fn rejects_bad_magic() {
        let mut g = good_bytes();
        g[0] = b'X';
        let r = SpatialIndex::open(Bytes::from(g));
        assert!(matches!(r, Err(ArtifactError::Malformed(_))));
    }

    #[test]
    fn rejects_unsupported_version() {
        let mut g = good_bytes();
        g[4] = 9;
        let r = SpatialIndex::open(Bytes::from(g));
        assert!(matches!(r, Err(ArtifactError::Malformed(_))));
    }

    #[test]
    fn rejects_nonzero_flags() {
        let mut g = good_bytes();
        g[5] = 1;
        let r = SpatialIndex::open(Bytes::from(g));
        assert!(matches!(r, Err(ArtifactError::Malformed(_))));
    }

    #[test]
    fn rejects_node_size_below_two_at_open() {
        // build a valid index, then poke node_size=1 in the header.
        let mut g = good_bytes();
        g[6] = 1;
        g[7] = 0;
        let r = SpatialIndex::open(Bytes::from(g));
        assert!(matches!(r, Err(ArtifactError::Malformed(_))));
    }

    #[test]
    fn builder_rejects_node_size_below_two() {
        let r = SpatialIndexBuilder::new(0);
        assert!(matches!(r, Err(ArtifactError::InvalidWriterState(_))));
        let r = SpatialIndexBuilder::new(1);
        assert!(matches!(r, Err(ArtifactError::InvalidWriterState(_))));
    }

    #[test]
    fn builder_rejects_non_finite_bbox() {
        let mut b = SpatialIndexBuilder::new(DEFAULT_NODE_SIZE).unwrap();
        b.add(0, [0.0, 0.0, 1.0, 1.0]).add(1, [f32::NAN, 0.0, 1.0, 1.0]);
        assert!(matches!(b.finish(), Err(ArtifactError::Malformed(_))));

        let mut b = SpatialIndexBuilder::new(DEFAULT_NODE_SIZE).unwrap();
        b.add(0, [f32::INFINITY, 0.0, 1.0, 1.0]);
        assert!(matches!(b.finish(), Err(ArtifactError::Malformed(_))));
    }

    #[test]
    fn rejects_nodes_region_truncation() {
        let g = good_bytes();
        // drop tail bytes one node-stride at a time; reader must not panic.
        for cut in 1..40 {
            let n = g.len() - cut;
            let r = SpatialIndex::open(Bytes::copy_from_slice(&g[..n]));
            assert!(r.is_err(), "expected error at tail-cut {cut}");
        }
    }

    #[test]
    fn rejects_nonmonotonic_level_offsets() {
        let mut g = good_bytes();
        // header is 36 bytes; level_offsets follow. write a deliberately
        // non-monotonic pair: level_starts[0] = 999_999, level_starts[1] = 0.
        g[36..40].copy_from_slice(&999_999u32.to_le_bytes());
        g[40..44].copy_from_slice(&0u32.to_le_bytes());
        let r = SpatialIndex::open(Bytes::from(g));
        assert!(matches!(r, Err(ArtifactError::Malformed(_))));
    }

    #[test]
    fn rejects_first_level_offset_nonzero() {
        let mut g = good_bytes();
        g[36..40].copy_from_slice(&20u32.to_le_bytes());
        let r = SpatialIndex::open(Bytes::from(g));
        assert!(matches!(r, Err(ArtifactError::Malformed(_))));
    }

    #[test]
    fn rejects_sentinel_mismatch_with_num_nodes() {
        // poke num_nodes way larger than reality; sentinel becomes inconsistent.
        let mut g = good_bytes();
        g[12..16].copy_from_slice(&999_999u32.to_le_bytes());
        let r = SpatialIndex::open(Bytes::from(g));
        assert!(matches!(
            r,
            Err(ArtifactError::Malformed(_)) | Err(ArtifactError::Truncated)
        ));
    }

    #[test]
    fn rejects_zero_levels_with_items() {
        let mut g = good_bytes();
        g[16..20].copy_from_slice(&0u32.to_le_bytes());
        let r = SpatialIndex::open(Bytes::from(g));
        assert!(matches!(r, Err(ArtifactError::Malformed(_))));
    }

    #[test]
    fn empty_index_rejects_nonzero_sentinel() {
        let bytes = build(&[], DEFAULT_NODE_SIZE).to_vec();
        // tamper sentinel at offset 36..40
        let mut g = bytes;
        g[36..40].copy_from_slice(&7u32.to_le_bytes());
        let r = SpatialIndex::open(Bytes::from(g));
        assert!(matches!(r, Err(ArtifactError::Malformed(_))));
    }
}
