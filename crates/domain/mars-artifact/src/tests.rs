use bytes::Bytes;
use mars_types::{Bbox, ContentHash};
use proptest::prelude::*;

use crate::{
    ArtifactError, ArtifactKind, ArtifactReader, ArtifactWriter, FORMAT_VERSION, MAGIC, SectionKind, SourceRef,
    compute_content_hash, decode_class_assignment, decode_geometry_payload, decode_style_refs, encode_geometry_payload,
};
use crate::{Coord, FeatureGeom, GeomKind};

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
    let bytes = crate::encode_class_assignment(&items);
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
    w.add_geometry_payload(&features)
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
    w.add_geometry_payload(&features)
        .set_bbox(Bbox::new(0.0, 0.0, 1.0, 1.0))
        .set_feature_count(99);
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
    let mut buf = crate::encode_class_assignment(&[(1u64, 0u16)]).to_vec();
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
