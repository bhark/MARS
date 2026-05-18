#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn point_le(x: f64, y: f64) -> Vec<u8> {
    let mut v = vec![1u8];
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&x.to_le_bytes());
    v.extend_from_slice(&y.to_le_bytes());
    v
}

fn polygon_le(rings: &[&[(f64, f64)]]) -> Vec<u8> {
    let mut v = vec![1u8];
    v.extend_from_slice(&3u32.to_le_bytes());
    v.extend_from_slice(&(rings.len() as u32).to_le_bytes());
    for ring in rings {
        v.extend_from_slice(&(ring.len() as u32).to_le_bytes());
        for (x, y) in *ring {
            v.extend_from_slice(&x.to_le_bytes());
            v.extend_from_slice(&y.to_le_bytes());
        }
    }
    v
}

fn linestring_le(pts: &[(f64, f64)]) -> Vec<u8> {
    let mut v = vec![1u8];
    v.extend_from_slice(&2u32.to_le_bytes());
    v.extend_from_slice(&(pts.len() as u32).to_le_bytes());
    for (x, y) in pts {
        v.extend_from_slice(&x.to_le_bytes());
        v.extend_from_slice(&y.to_le_bytes());
    }
    v
}

#[test]
fn point_decode() {
    let g = wkb_to_feature_geom(&point_le(1.5, 2.5), 7).unwrap();
    assert_eq!(g.user_id, 7);
    assert!(matches!(g.geom, GeomKind::Point((1.5, 2.5))));
}

#[test]
fn linestring_decode() {
    let g = wkb_to_feature_geom(&linestring_le(&[(0.0, 0.0), (1.0, 2.0)]), 1).unwrap();
    match g.geom {
        GeomKind::LineString(coords) => assert_eq!(coords, vec![(0.0, 0.0), (1.0, 2.0)]),
        other => panic!("unexpected: {other:?}"),
    }
    assert_eq!(g.bbox, [0.0_f32, 0.0, 1.0, 2.0]);
}

#[test]
fn polygon_decode_multi_ring() {
    let p = polygon_le(&[
        &[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)],
        &[(2.0, 2.0), (8.0, 2.0), (8.0, 8.0), (2.0, 8.0), (2.0, 2.0)],
    ]);
    let g = wkb_to_feature_geom(&p, 11).unwrap();
    match g.geom {
        GeomKind::Polygon(rings) => {
            assert_eq!(rings.len(), 2);
            assert_eq!(rings[0].len(), 5);
            assert_eq!(rings[1][0], (2.0, 2.0));
        }
        other => panic!("unexpected: {other:?}"),
    }
    assert_eq!(g.bbox[2], 10.0);
}

#[test]
fn multipolygon_decode() {
    let mut v = vec![1u8];
    v.extend_from_slice(&6u32.to_le_bytes());
    v.extend_from_slice(&2u32.to_le_bytes());
    v.extend_from_slice(&polygon_le(&[&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)]]));
    v.extend_from_slice(&polygon_le(&[&[
        (10.0, 10.0),
        (12.0, 10.0),
        (12.0, 12.0),
        (10.0, 10.0),
    ]]));
    let g = wkb_to_feature_geom(&v, 1).unwrap();
    match g.geom {
        GeomKind::MultiPolygon(polys) => assert_eq!(polys.len(), 2),
        other => panic!("unexpected: {other:?}"),
    }
    assert_eq!(g.bbox, [0.0_f32, 0.0, 12.0, 12.0]);
}

#[test]
fn bbox_only_works_too() {
    let bb = wkb_bbox(&point_le(3.0, 4.0)).unwrap();
    assert_eq!((bb.min_x, bb.min_y), (3.0, 4.0));
}

#[test]
fn empty_polygon_errors() {
    assert!(matches!(wkb_to_feature_geom(&polygon_le(&[]), 0), Err(WkbError::Empty)));
}

#[test]
fn unsupported_type_rejected() {
    let mut v = vec![1u8];
    v.extend_from_slice(&7u32.to_le_bytes());
    assert!(matches!(wkb_to_feature_geom(&v, 0), Err(WkbError::UnsupportedType(7))));
}

#[test]
fn truncated_rejected() {
    let mut v = point_le(0.0, 0.0);
    v.truncate(v.len() - 4);
    assert!(matches!(wkb_to_feature_geom(&v, 0), Err(WkbError::Truncated)));
}

#[test]
fn wkb_centroid_returns_bbox_midpoint() {
    // point centroid is the point itself.
    assert_eq!(wkb_centroid(&point_le(7.0, 7.0)).unwrap(), [7.0, 7.0]);
    // linestring centroid is the midpoint of the bbox, not the path centroid.
    let centroid = wkb_centroid(&linestring_le(&[(0.0, 0.0), (10.0, 4.0)])).unwrap();
    assert_eq!(centroid, [5.0, 2.0]);
}

#[test]
fn wkb_centroid_propagates_empty() {
    assert!(matches!(wkb_centroid(&polygon_le(&[])), Err(WkbError::Empty)));
}

#[test]
fn ewkb_with_srid_decodes() {
    let mut v = vec![1u8];
    v.extend_from_slice(&(1u32 | EWKB_SRID_FLAG).to_le_bytes());
    v.extend_from_slice(&25832u32.to_le_bytes());
    v.extend_from_slice(&5.0_f64.to_le_bytes());
    v.extend_from_slice(&6.0_f64.to_le_bytes());
    let g = wkb_to_feature_geom(&v, 0).unwrap();
    assert!(matches!(g.geom, GeomKind::Point((5.0, 6.0))));
}
