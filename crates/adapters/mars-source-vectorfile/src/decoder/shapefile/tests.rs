#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write;

use super::*;
use shapefile::{Point, dbase::FieldName};

fn synth_shapefile_zip() -> Vec<u8> {
    // build two-point shapefile via the shapefile writer, then zip the
    // .shp / .shx / .dbf into a single archive.
    let mut shp_buf: Vec<u8> = Vec::new();
    let mut shx_buf: Vec<u8> = Vec::new();
    let mut dbf_buf: Vec<u8> = Vec::new();

    let shape_writer = shapefile::ShapeWriter::with_shx(Cursor::new(&mut shp_buf), Cursor::new(&mut shx_buf));
    let table = dbase::TableWriterBuilder::new()
        .add_character_field(FieldName::try_from("name").unwrap(), 16)
        .add_numeric_field(FieldName::try_from("score").unwrap(), 10, 2);
    let dbase_writer = table.build_with_dest(Cursor::new(&mut dbf_buf));
    let mut writer = shapefile::Writer::new(shape_writer, dbase_writer);

    let mut rec_a = dbase::Record::default();
    rec_a.insert(
        "name".to_string(),
        dbase::FieldValue::Character(Some("alpha".to_string())),
    );
    rec_a.insert("score".to_string(), dbase::FieldValue::Numeric(Some(1.5)));
    writer.write_shape_and_record(&Point::new(1.0, 2.0), &rec_a).unwrap();

    let mut rec_b = dbase::Record::default();
    rec_b.insert(
        "name".to_string(),
        dbase::FieldValue::Character(Some("beta".to_string())),
    );
    rec_b.insert("score".to_string(), dbase::FieldValue::Numeric(Some(42.0)));
    writer.write_shape_and_record(&Point::new(3.0, 4.0), &rec_b).unwrap();
    drop(writer);

    let mut zip_buf: Vec<u8> = Vec::new();
    {
        let mut zw = zip::ZipWriter::new(Cursor::new(&mut zip_buf));
        let opts: zip::write::SimpleFileOptions =
            zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zw.start_file("pts.shp", opts).unwrap();
        zw.write_all(&shp_buf).unwrap();
        zw.start_file("pts.shx", opts).unwrap();
        zw.write_all(&shx_buf).unwrap();
        zw.start_file("pts.dbf", opts).unwrap();
        zw.write_all(&dbf_buf).unwrap();
        zw.finish().unwrap();
    }
    zip_buf
}

#[test]
fn decodes_known_shapefile_zip() {
    let zip = synth_shapefile_zip();
    // fixture stays well below 5 KB:
    assert!(
        zip.len() < 5 * 1024,
        "synth zip should be tiny (got {} bytes)",
        zip.len()
    );

    let mut got: Vec<DecodedFeature> = Vec::new();
    ShapefileDecoder
        .decode(&Bytes::from(zip), &mut |f| {
            got.push(f);
            true
        })
        .unwrap();
    assert_eq!(got.len(), 2);

    // wkb sanity: byte_order(1) + type(4) + 2 doubles for points
    for f in &got {
        assert_eq!(f.geometry_wkb.len(), 1 + 4 + 16);
        assert_eq!(f.geometry_wkb[0], 1); // little-endian
    }

    // attribute round-trip
    let mut names: Vec<_> = got
        .iter()
        .filter_map(|f| match f.attributes.get("name") {
            Some(AttrValue::String(s)) => Some(s.trim().to_string()),
            _ => None,
        })
        .collect();
    names.sort();
    assert_eq!(names, vec!["alpha", "beta"]);
    // numeric round-trip (dbase Numeric -> AttrValue::Float)
    assert!(matches!(got[0].attributes.get("score"), Some(AttrValue::Float(_))));
}

#[test]
fn rejects_zip_missing_shx() {
    // build a zip with only .shp + .dbf (no .shx) and expect Schema err.
    let mut buf = Vec::new();
    {
        let mut zw = zip::ZipWriter::new(Cursor::new(&mut buf));
        let opts: zip::write::SimpleFileOptions =
            zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zw.start_file("a.shp", opts).unwrap();
        zw.write_all(b"not real but contents irrelevant").unwrap();
        zw.start_file("a.dbf", opts).unwrap();
        zw.write_all(b"x").unwrap();
        zw.finish().unwrap();
    }
    let err = ShapefileDecoder.decode(&Bytes::from(buf), &mut |_| true).unwrap_err();
    assert!(
        matches!(err, DecoderError::Schema(ref s) if s.contains(".shx")),
        "got {err:?}"
    );
}

#[test]
fn rejects_zip_with_basename_mismatch() {
    // shp basename 'a', dbf basename 'b' - mismatched triple.
    let mut buf = Vec::new();
    {
        let mut zw = zip::ZipWriter::new(Cursor::new(&mut buf));
        let opts: zip::write::SimpleFileOptions =
            zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zw.start_file("a.shp", opts).unwrap();
        zw.write_all(b"x").unwrap();
        zw.start_file("a.shx", opts).unwrap();
        zw.write_all(b"x").unwrap();
        zw.start_file("b.dbf", opts).unwrap();
        zw.write_all(b"x").unwrap();
        zw.finish().unwrap();
    }
    let err = ShapefileDecoder.decode(&Bytes::from(buf), &mut |_| true).unwrap_err();
    assert!(
        matches!(err, DecoderError::Schema(ref s) if s.contains("does not match")),
        "got {err:?}"
    );
}
