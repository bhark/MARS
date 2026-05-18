#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn parses_format_and_crs_from_fragment() {
    let p = parse("file:///x.fgb#format=flat_geobuf&source_crs=EPSG:4326").unwrap();
    assert_eq!(p.uri, "file:///x.fgb");
    assert_eq!(p.format, VectorFileFormat::FlatGeobuf);
    assert_eq!(p.source_crs.unwrap().as_str(), "EPSG:4326");
}

#[test]
fn falls_back_to_extension_inference() {
    let p = parse("s3://bucket/data/roads.fgb").unwrap();
    assert_eq!(p.uri, "s3://bucket/data/roads.fgb");
    assert_eq!(p.format, VectorFileFormat::FlatGeobuf);
    assert!(p.source_crs.is_none());

    let p = parse("https://example.org/data.geojson").unwrap();
    assert_eq!(p.format, VectorFileFormat::GeoJson);
}

#[test]
fn empty_fragment_value_rejected() {
    let err = parse("file:///x.fgb#format=").unwrap_err();
    assert!(matches!(err, FragmentError::EmptyValue("format")));
}

#[test]
fn unknown_key_rejected() {
    let err = parse("file:///x.fgb#weird=1").unwrap_err();
    assert!(matches!(err, FragmentError::UnknownKey(k) if k == "weird"));
}

#[test]
fn undecidable_extension_rejected() {
    let err = parse("file:///opaque").unwrap_err();
    assert!(matches!(err, FragmentError::UndecidableFormat(_)));
}

#[test]
fn accepts_alternate_spellings() {
    assert_eq!(parse("u#format=fgb").unwrap().format, VectorFileFormat::FlatGeobuf);
    assert_eq!(parse("u#format=geo_json").unwrap().format, VectorFileFormat::GeoJson);
    // shapefile spellings only pass when the URI is a recognised archive.
    assert_eq!(
        parse("u.shp.zip#format=shp").unwrap().format,
        VectorFileFormat::Shapefile
    );
    assert_eq!(
        parse("u.shz#format=shapefile").unwrap().format,
        VectorFileFormat::Shapefile
    );
}

#[test]
fn infers_shapefile_compound_extension() {
    let p = parse("s3://b/data/roads.shp.zip").unwrap();
    assert_eq!(p.format, VectorFileFormat::Shapefile);
    let p = parse("file:///x.shz").unwrap();
    assert_eq!(p.format, VectorFileFormat::Shapefile);
}

#[test]
fn rejects_raw_shp_uri() {
    let err = parse("s3://bucket/data/roads.shp").unwrap_err();
    match err {
        FragmentError::UnsupportedRawShapefile(uri) => {
            assert_eq!(uri, "s3://bucket/data/roads.shp");
        }
        other => panic!("expected UnsupportedRawShapefile, got {other:?}"),
    }
}

#[test]
fn rejects_forced_shapefile_on_non_archive_uri() {
    let err = parse("file:///opaque.bin#format=shapefile").unwrap_err();
    assert!(matches!(err, FragmentError::UnsupportedRawShapefile(_)));
}
