#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn point_wkb_le(x: f64, y: f64) -> Vec<u8> {
    let mut v = Vec::with_capacity(21);
    v.push(1u8); // little-endian
    v.extend_from_slice(&WKB_POINT.to_le_bytes());
    v.extend_from_slice(&x.to_le_bytes());
    v.extend_from_slice(&y.to_le_bytes());
    v
}

#[test]
fn wkb_point_in_epsg_4326_reprojects_to_25832() {
    // copenhagen-ish.
    let wkb = point_wkb_le(12.5683, 55.6761);
    let xform = transformer(&CrsCode::new("EPSG:4326"), &CrsCode::new("EPSG:25832")).unwrap();
    let out = reproject_wkb(&wkb, &xform).unwrap();
    // strip header and read back the projected coords.
    assert_eq!(out[0], 1);
    let mut a = [0u8; 4];
    a.copy_from_slice(&out[1..5]);
    assert_eq!(u32::from_le_bytes(a), WKB_POINT);
    let mut xa = [0u8; 8];
    xa.copy_from_slice(&out[5..13]);
    let mut ya = [0u8; 8];
    ya.copy_from_slice(&out[13..21]);
    let x = f64::from_le_bytes(xa);
    let y = f64::from_le_bytes(ya);
    // copenhagen in utm 32n sits around (725_000, 6_180_000) m.
    assert!((x - 725_000.0).abs() < 5_000.0, "x = {x}");
    assert!((y - 6_180_000.0).abs() < 5_000.0, "y = {y}");
}

#[test]
fn wkb_polygon_roundtrips_coord_count() {
    // square ring; identity reproject (epsg:25832 -> epsg:25832) so
    // we can verify the structural roundtrip without proj noise.
    let mut wkb = Vec::new();
    wkb.push(1u8);
    wkb.extend_from_slice(&WKB_POLYGON.to_le_bytes());
    wkb.extend_from_slice(&1u32.to_le_bytes()); // 1 ring
    wkb.extend_from_slice(&5u32.to_le_bytes()); // 5 points
    for (x, y) in [(0.0_f64, 0.0_f64), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)] {
        wkb.extend_from_slice(&x.to_le_bytes());
        wkb.extend_from_slice(&y.to_le_bytes());
    }
    let xform = transformer(&CrsCode::new("EPSG:25832"), &CrsCode::new("EPSG:25832")).unwrap();
    let out = reproject_wkb(&wkb, &xform).unwrap();
    assert_eq!(out.len(), wkb.len());
}

#[test]
fn wkb_rejects_unsupported_z() {
    // type with Z bit set (0x80000001) -> error.
    let mut wkb = Vec::new();
    wkb.push(1u8);
    wkb.extend_from_slice(&0x8000_0001u32.to_le_bytes());
    wkb.extend_from_slice(&0.0f64.to_le_bytes());
    wkb.extend_from_slice(&0.0f64.to_le_bytes());
    wkb.extend_from_slice(&0.0f64.to_le_bytes()); // z
    let xform = transformer(&CrsCode::new("EPSG:4326"), &CrsCode::new("EPSG:25832")).unwrap();
    let err = reproject_wkb(&wkb, &xform).unwrap_err();
    assert!(matches!(err, ReprojectError::Wkb(_)));
}
