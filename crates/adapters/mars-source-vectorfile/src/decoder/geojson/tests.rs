#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn decodes_known_geojson() {
    let s = r#"{
            "type": "FeatureCollection",
            "features": [
              {
                "type": "Feature",
                "id": 7,
                "geometry": {"type":"Point","coordinates":[10.0,20.0]},
                "properties": {"name":"alpha", "count": 42, "active": true}
              },
              {
                "type": "Feature",
                "geometry": {"type":"Point","coordinates":[1.0,2.0]},
                "properties": {"name":"beta", "count": 1.5}
              }
            ]
        }"#;
    let mut got: Vec<DecodedFeature> = Vec::new();
    GeoJsonDecoder
        .decode(&Bytes::from(s.as_bytes().to_vec()), &mut |f| {
            got.push(f);
            true
        })
        .unwrap();
    assert_eq!(got.len(), 2);
    // first feature has explicit id
    assert_eq!(got[0].feature_id, 7);
    assert_eq!(got[1].feature_id, 0); // fallback to row index downstream

    // wkb sanity
    assert!(!got[0].geometry_wkb.is_empty());
    assert_eq!(got[0].geometry_wkb[0], 1); // little-endian

    // attribute decoding
    assert!(matches!(got[0].attributes.get("name"), Some(AttrValue::String(s)) if s == "alpha"));
    assert!(matches!(got[0].attributes.get("count"), Some(AttrValue::Int(42))));
    assert!(matches!(got[0].attributes.get("active"), Some(AttrValue::Bool(true))));
    assert!(matches!(got[1].attributes.get("count"), Some(AttrValue::Float(f)) if (*f - 1.5).abs() < 1e-9));
}

#[test]
fn skips_null_geometry() {
    let s = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":null,"properties":{}}
        ]}"#;
    let mut got = 0;
    GeoJsonDecoder
        .decode(&Bytes::from(s.as_bytes().to_vec()), &mut |_| {
            got += 1;
            true
        })
        .unwrap();
    assert_eq!(got, 0);
}

#[test]
fn single_feature_root() {
    let s = r#"{"type":"Feature","id":3,"geometry":{"type":"Point","coordinates":[0,0]},"properties":{}}"#;
    let mut got: Vec<DecodedFeature> = Vec::new();
    GeoJsonDecoder
        .decode(&Bytes::from(s.as_bytes().to_vec()), &mut |f| {
            got.push(f);
            true
        })
        .unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].feature_id, 3);
}
