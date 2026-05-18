#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::fs::File;
use std::io::Read;

/// Build a minimal GeoPackage on disk with one feature table holding
/// two point features. Returns the file bytes so the decoder can
/// consume the same shape it would see from the object-store fetcher.
fn synth_geopackage_bytes() -> Vec<u8> {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let conn = Connection::open(tmp.path()).unwrap();
    conn.execute_batch(
        "BEGIN;
             CREATE TABLE gpkg_contents (
                 table_name TEXT NOT NULL PRIMARY KEY,
                 data_type TEXT NOT NULL,
                 identifier TEXT,
                 description TEXT,
                 last_change DATETIME,
                 min_x DOUBLE,
                 min_y DOUBLE,
                 max_x DOUBLE,
                 max_y DOUBLE,
                 srs_id INTEGER
             );
             CREATE TABLE gpkg_geometry_columns (
                 table_name TEXT NOT NULL PRIMARY KEY,
                 column_name TEXT NOT NULL,
                 geometry_type_name TEXT NOT NULL,
                 srs_id INTEGER NOT NULL,
                 z TINYINT NOT NULL,
                 m TINYINT NOT NULL
             );
             CREATE TABLE pois (
                 fid INTEGER PRIMARY KEY AUTOINCREMENT,
                 geom BLOB,
                 name TEXT,
                 score REAL
             );
             INSERT INTO gpkg_contents(table_name, data_type, srs_id)
                 VALUES ('pois', 'features', 25832);
             INSERT INTO gpkg_geometry_columns(table_name, column_name, geometry_type_name, srs_id, z, m)
                 VALUES ('pois', 'geom', 'POINT', 25832, 0, 0);
             COMMIT;",
    )
    .unwrap();

    // build two point geometries as GeoPackageBinary: GP + version 0 +
    // flags=0x01 (little-endian, no envelope) + srs_id i32 LE + WKB.
    let pt_a = point_blob(25832, 10.0, 20.0);
    let pt_b = point_blob(25832, 30.0, 40.0);

    conn.execute(
        "INSERT INTO pois(geom, name, score) VALUES (?, 'alpha', 1.5)",
        rusqlite::params![pt_a],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO pois(geom, name, score) VALUES (?, 'beta', 42.0)",
        rusqlite::params![pt_b],
    )
    .unwrap();
    drop(conn);

    let mut buf = Vec::new();
    File::open(tmp.path()).unwrap().read_to_end(&mut buf).unwrap();
    buf
}

/// Encode a POINT as a GeoPackageBinary blob: GP magic + version 0 +
/// flags (little-endian, no envelope) + srs_id (i32 LE) + WKB(POINT).
fn point_blob(srs_id: i32, x: f64, y: f64) -> Vec<u8> {
    let mut blob = Vec::with_capacity(8 + 21);
    blob.extend_from_slice(b"GP");
    blob.push(0); // version
    blob.push(0b0000_0001); // flags: LE byte order, no envelope
    blob.extend_from_slice(&srs_id.to_le_bytes());
    // WKB POINT: byte order(1)=LE, type(4)=1 (Point), 2*f64 coords
    blob.push(0x01);
    blob.extend_from_slice(&1u32.to_le_bytes());
    blob.extend_from_slice(&x.to_le_bytes());
    blob.extend_from_slice(&y.to_le_bytes());
    blob
}

#[test]
fn decodes_two_features_with_attributes() {
    let bytes = synth_geopackage_bytes();

    let mut got: Vec<DecodedFeature> = Vec::new();
    GeoPackageDecoder
        .decode(&Bytes::from(bytes), &mut |f| {
            got.push(f);
            true
        })
        .unwrap();
    assert_eq!(got.len(), 2);

    // wkb sanity: byte_order(1) + type(4) + 2*f64
    for f in &got {
        assert_eq!(f.geometry_wkb.len(), 1 + 4 + 16);
        assert_eq!(f.geometry_wkb[0], 1);
    }

    // pk round-trips through the fid INTEGER PRIMARY KEY.
    let mut ids: Vec<u64> = got.iter().map(|f| f.feature_id).collect();
    ids.sort();
    assert_eq!(ids, vec![1, 2]);

    // attribute round-trip
    let names: Vec<_> = got
        .iter()
        .filter_map(|f| match f.attributes.get("name") {
            Some(AttrValue::String(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert!(names.contains(&"alpha".to_string()));
    assert!(names.contains(&"beta".to_string()));
}

#[test]
fn rejects_blob_without_gp_magic() {
    let err = strip_gpkg_header(&[0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0x01]).unwrap_err();
    assert!(matches!(err, DecoderError::Schema(ref s) if s.contains("magic")));
}

#[test]
fn rejects_blob_shorter_than_header() {
    let err = strip_gpkg_header(b"GP").unwrap_err();
    assert!(matches!(err, DecoderError::Schema(_)));
}

#[test]
fn rejects_extended_binary_type() {
    // flags bit 5 = extended geopackage binary
    let blob = [b'G', b'P', 0, 0b0010_0000, 0, 0, 0, 0];
    let err = strip_gpkg_header(&blob).unwrap_err();
    assert!(matches!(err, DecoderError::Schema(ref s) if s.contains("extended")));
}

#[test]
fn skips_envelope_bytes_per_flags() {
    // flags envelope-indicator=1 (xy, 32 envelope bytes), LE byte order
    let mut blob = vec![b'G', b'P', 0, 0b0000_0011];
    blob.extend_from_slice(&25832i32.to_le_bytes());
    blob.extend_from_slice(&[0u8; 32]); // envelope placeholder
    // a 21-byte WKB POINT body
    blob.push(0x01);
    blob.extend_from_slice(&1u32.to_le_bytes());
    blob.extend_from_slice(&0f64.to_le_bytes());
    blob.extend_from_slice(&0f64.to_le_bytes());
    let wkb = strip_gpkg_header(&blob).unwrap();
    assert_eq!(wkb.len(), 21);
    assert_eq!(wkb[0], 1);
}

#[test]
fn rejects_identifier_with_special_chars() {
    assert!(validate_identifier("ok_name").is_ok());
    assert!(validate_identifier("with-dash").is_ok());
    assert!(validate_identifier("with\"quote").is_err());
    assert!(validate_identifier("space inside").is_err());
    assert!(validate_identifier("").is_err());
}
