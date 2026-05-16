//! shared fixtures for vectorfile wiremock tests. each test file pulls this
//! via `#[path = "common/mod.rs"] mod common;` to avoid the implicit-mod
//! unused-warning issue when individual fns aren't called by every file.

use std::sync::Arc;

use bytes::Bytes;
use flatgeobuf::{ColumnType, FgbWriter, FgbWriterOptions, GeometryType};
use futures_util::TryStreamExt;
use geozero::geojson::GeoJson;
use geozero::{ColumnValue, PropertyProcessor};
use mars_config::{SourceId, VectorFileBackend, VectorFileFormat};
use mars_source::{RowBytes, Source, SourceBinding};
use mars_source_vectorfile::VectorFileSource;
use mars_types::{CrsCode, SourceCollectionId};
use wiremock::MockServer;

/// build a tiny three-point FlatGeobuf payload at runtime. avoids checking
/// binary fixtures into the repo. coords are in the binding's `source_crs`;
/// the adapter reprojects to `native_crs` at decode time.
pub(crate) fn build_three_points_fgb() -> Vec<u8> {
    let mut fgb = FgbWriter::create_with_options(
        "test_points",
        GeometryType::Point,
        FgbWriterOptions {
            // keep geometry strictly as Point so the round-tripped WKB stays
            // 21 bytes and `parse_wkb_point` works.
            write_index: false,
            detect_type: false,
            promote_to_multi: false,
            ..Default::default()
        },
    )
    .expect("fgb writer");
    fgb.add_column("name", ColumnType::String, |_, _| {});

    for (idx, (x, y, name)) in [(0.0_f64, 0.0_f64, "a"), (10.0, 10.0, "b"), (20.0, 20.0, "c")]
        .into_iter()
        .enumerate()
    {
        let gj = format!(r#"{{"type":"Point","coordinates":[{x},{y}]}}"#);
        let geom = GeoJson(&gj);
        fgb.add_feature_geom(geom, |feat| {
            feat.property(0, "name", &ColumnValue::String(name)).ok();
            let _ = idx;
        })
        .expect("add feature");
    }
    let mut out = Vec::new();
    fgb.write(&mut out).expect("write fgb");
    out
}

/// minimal single-point GeoJSON FeatureCollection. attribute and id both
/// integer so the value_to_attr / native-fid paths are exercised.
pub(crate) fn single_point_geojson(x: f64, y: f64) -> String {
    format!(
        r#"{{"type":"FeatureCollection","features":[{{"type":"Feature","id":42,"properties":{{"name":"alpha","rank":7}},"geometry":{{"type":"Point","coordinates":[{x},{y}]}}}}]}}"#
    )
}

/// construct a vectorfile source over a fresh tempdir cache. enables
/// `allow_http` so wiremock's plain http listener is reachable.
pub(crate) async fn source_with_temp_cache(native_crs: &str) -> (VectorFileSource, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = VectorFileBackend {
        cache_dir: tmp.path().display().to_string(),
        allow_http: true,
        ..Default::default()
    };
    let src = VectorFileSource::new(SourceId::new("vf-test"), CrsCode::new(native_crs), cfg)
        .await
        .expect("vectorfile source");
    (src, tmp)
}

pub(crate) fn binding_for(
    server: &MockServer,
    path: &str,
    format: VectorFileFormat,
    source_crs: &str,
) -> SourceBinding {
    let fmt_tag = match format {
        VectorFileFormat::FlatGeobuf => "flat_geobuf",
        VectorFileFormat::GeoJson => "geo_json",
        VectorFileFormat::Shapefile => "shapefile",
        VectorFileFormat::GeoPackage => "geo_package",
    };
    let from = format!("{}{}#format={}&source_crs={}", server.uri(), path, fmt_tag, source_crs);
    SourceBinding::new(
        SourceCollectionId::new("collection_under_test"),
        from,
        "geom",
        "id",
        vec!["name".into()],
        CrsCode::new(source_crs),
    )
    .expect("binding")
}

/// drive `stream_rows` to completion and return all rows.
pub(crate) async fn collect_rows(src: Arc<VectorFileSource>, binding: SourceBinding) -> Vec<RowBytes> {
    let stream = src.stream_rows(&binding).await.expect("stream_rows");
    stream.try_collect::<Vec<_>>().await.expect("rows collect")
}

/// parse a little-endian point WKB. only used to verify reprojection moved
/// the coordinates; doesn't try to be a complete WKB parser.
pub(crate) fn parse_wkb_point(wkb: &Bytes) -> (f64, f64) {
    assert_eq!(wkb.len(), 21, "expected 21-byte point WKB, got {} bytes", wkb.len());
    assert_eq!(wkb[0], 1, "expected little-endian wkb (byte 0 = 1)");
    let geom_type = u32::from_le_bytes([wkb[1], wkb[2], wkb[3], wkb[4]]);
    assert_eq!(geom_type, 1, "expected geometry type 1 (Point)");
    let mut x = [0u8; 8];
    let mut y = [0u8; 8];
    x.copy_from_slice(&wkb[5..13]);
    y.copy_from_slice(&wkb[13..21]);
    (f64::from_le_bytes(x), f64::from_le_bytes(y))
}
