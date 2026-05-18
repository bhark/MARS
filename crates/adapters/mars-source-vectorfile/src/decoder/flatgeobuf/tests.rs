#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use flatgeobuf::FgbWriter;

fn synth_fgb() -> Vec<u8> {
    // build a tiny dataset with two point features and one string attr.
    let mut w = FgbWriter::create("pts", flatgeobuf::GeometryType::Point).unwrap();
    w.add_column("name", ColumnType::String, |_, col| {
        col.nullable = true;
    });

    let geojson =
        r#"{"type":"Feature","geometry":{"type":"Point","coordinates":[1.0,2.0]},"properties":{"name":"alpha"}}"#;
    w.add_feature(geozero::geojson::GeoJson(geojson)).unwrap();
    let geojson2 =
        r#"{"type":"Feature","geometry":{"type":"Point","coordinates":[3.0,4.0]},"properties":{"name":"beta"}}"#;
    w.add_feature(geozero::geojson::GeoJson(geojson2)).unwrap();

    let mut buf = Vec::new();
    w.write(&mut buf).unwrap();
    buf
}

#[test]
fn decodes_known_fgb() {
    let buf = synth_fgb();
    let decoder = FlatGeobufDecoder;
    let mut collected: Vec<DecodedFeature> = Vec::new();
    decoder
        .decode(&Bytes::from(buf), &mut |f| {
            collected.push(f);
            true
        })
        .unwrap();
    assert_eq!(collected.len(), 2);
    // fgb writer reorders by spatial index, so we don't assume insert order.
    let mut names: Vec<_> = collected
        .iter()
        .filter_map(|f| match f.attributes.get("name") {
            Some(AttrValue::String(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    names.sort();
    assert_eq!(names, vec!["alpha", "beta"]);
    // wkb sanity: byte_order(1) + type(4) + 2 doubles
    for f in &collected {
        assert_eq!(f.geometry_wkb.len(), 1 + 4 + 16);
        assert_eq!(f.geometry_wkb[0], 1); // little endian
    }
}
