//! end-to-end coverage of `VectorFileSource::stream_rows` against a real
//! `object_store::http` HEAD+GET round-trip via wiremock. complements the
//! in-module fragment + cache unit tests by pinning the full
//! fetch -> head -> cache-probe -> get -> decode -> reproject pipeline.
//!
//! HTTP, not HTTPS - wiremock serves plain http; object_store::http accepts
//! both. the test names use `http` to avoid lying about the scheme.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;

use mars_config::VectorFileFormat;
use mars_source::{Source, SourceError};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{
    binding_for, build_three_points_fgb, collect_rows, parse_wkb_point, single_point_geojson, source_with_temp_cache,
};

/// helper: mount a paired HEAD + GET against the same path. HEAD returns
/// Content-Length + (optional) ETag; GET returns body + ETag.
async fn mount_object(
    server: &MockServer,
    path_str: &str,
    body: Vec<u8>,
    etag: Option<&str>,
    head_expect: u64,
    get_expect: u64,
) {
    let body_len = body.len();
    let head_t = {
        let mut t = ResponseTemplate::new(200).insert_header("Content-Length", body_len.to_string().as_str());
        if let Some(e) = etag {
            t = t.insert_header("ETag", e);
        }
        t
    };
    let get_t = {
        let mut t = ResponseTemplate::new(200).set_body_bytes(body);
        if let Some(e) = etag {
            t = t.insert_header("ETag", e);
        }
        t
    };
    Mock::given(method("HEAD"))
        .and(path(path_str))
        .respond_with(head_t)
        .expect(head_expect)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(path_str))
        .respond_with(get_t)
        .expect(get_expect)
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn http_get_decodes_geojson_with_attribute_passthrough() {
    let server = MockServer::start().await;
    let body = single_point_geojson(0.0, 0.0).into_bytes();
    mount_object(&server, "/single.geojson", body, Some("v1"), 1, 1).await;

    let (src, _tmp) = source_with_temp_cache("EPSG:25832").await;
    let binding = binding_for(&server, "/single.geojson", VectorFileFormat::GeoJson, "EPSG:25832");
    let rows = collect_rows(Arc::new(src), binding).await;

    assert_eq!(rows.len(), 1, "expected one feature, got {}", rows.len());
    let row = &rows[0];
    assert_eq!(row.feature_id, 42, "GeoJSON top-level id must surface as feature_id");
    let name = row.attributes.iter().find(|(k, _)| k == "name").map(|(_, v)| v.clone());
    assert!(matches!(name, Some(mars_source::AttrValue::String(ref s)) if s == "alpha"));
}

#[tokio::test(flavor = "multi_thread")]
async fn http_get_decodes_flatgeobuf_to_three_row_stream() {
    let server = MockServer::start().await;
    let fgb = build_three_points_fgb();
    mount_object(&server, "/three.fgb", fgb, Some("v1"), 1, 1).await;

    let (src, _tmp) = source_with_temp_cache("EPSG:25832").await;
    // source_crs matches native_crs so no reprojection happens; we're only
    // proving decode wiring here.
    let binding = binding_for(&server, "/three.fgb", VectorFileFormat::FlatGeobuf, "EPSG:25832");
    let rows = collect_rows(Arc::new(src), binding).await;

    assert_eq!(rows.len(), 3, "expected three features, got {}", rows.len());
    for r in &rows {
        // Point WKB is always 21 bytes; assert geometry survived end-to-end.
        assert_eq!(r.geometry.len(), 21, "expected 21-byte point WKB");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cache_hit_skips_origin_get_on_second_call() {
    let server = MockServer::start().await;
    let body = single_point_geojson(0.0, 0.0).into_bytes();
    // HEAD is called on every fetch_cached invocation; GET only on cache miss.
    mount_object(&server, "/cache.geojson", body, Some("v1"), 2, 1).await;

    let (src, _tmp) = source_with_temp_cache("EPSG:25832").await;
    let binding = binding_for(&server, "/cache.geojson", VectorFileFormat::GeoJson, "EPSG:25832");
    let src = Arc::new(src);
    let _ = collect_rows(src.clone(), binding.clone()).await;
    let _ = collect_rows(src, binding).await;
    // expectations are verified at server drop.
}

#[tokio::test(flavor = "multi_thread")]
async fn etag_change_invalidates_cache_and_refetches() {
    let server = MockServer::start().await;
    let body_v1 = single_point_geojson(0.0, 0.0).into_bytes();
    let body_v2 = single_point_geojson(1.0, 1.0).into_bytes();

    // first window: head + get with etag v1. expect exactly one of each.
    Mock::given(method("HEAD"))
        .and(path("/etag.geojson"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("ETag", "v1")
                .insert_header("Content-Length", body_v1.len().to_string().as_str()),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/etag.geojson"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("ETag", "v1")
                .set_body_bytes(body_v1),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    // second window: head + get with etag v2. expect exactly one of each.
    Mock::given(method("HEAD"))
        .and(path("/etag.geojson"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("ETag", "v2")
                .insert_header("Content-Length", body_v2.len().to_string().as_str()),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/etag.geojson"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("ETag", "v2")
                .set_body_bytes(body_v2),
        )
        .expect(1)
        .mount(&server)
        .await;

    let (src, _tmp) = source_with_temp_cache("EPSG:25832").await;
    let binding = binding_for(&server, "/etag.geojson", VectorFileFormat::GeoJson, "EPSG:25832");
    let src = Arc::new(src);
    let _ = collect_rows(src.clone(), binding.clone()).await;
    let _ = collect_rows(src, binding).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn head_without_etag_still_caches_under_unknown_key() {
    let server = MockServer::start().await;
    let body = single_point_geojson(0.0, 0.0).into_bytes();
    // HEAD has no ETag; the adapter assigns the synthetic "unknown" key.
    // Two calls -> 2 HEADs, 1 GET; the second call hits cache.
    let len = body.len();
    Mock::given(method("HEAD"))
        .and(path("/noetag.geojson"))
        .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", len.to_string().as_str()))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/noetag.geojson"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .expect(1)
        .mount(&server)
        .await;

    let (src, _tmp) = source_with_temp_cache("EPSG:25832").await;
    let binding = binding_for(&server, "/noetag.geojson", VectorFileFormat::GeoJson, "EPSG:25832");
    let src = Arc::new(src);
    let _ = collect_rows(src.clone(), binding.clone()).await;
    let _ = collect_rows(src, binding).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reproject_from_epsg_4326_to_epsg_25832_moves_coords_off_lonlat() {
    let server = MockServer::start().await;
    // a recognizable Denmark-ish lon/lat (10.0, 56.0) in EPSG:4326. after
    // reprojection to EPSG:25832 (UTM zone 32N), values are on the order of
    // 500k east / 6.2M north - nowhere near 10 / 56.
    let body = single_point_geojson(10.0, 56.0).into_bytes();
    mount_object(&server, "/reproject.geojson", body, Some("v1"), 1, 1).await;

    let (src, _tmp) = source_with_temp_cache("EPSG:25832").await;
    let binding = binding_for(&server, "/reproject.geojson", VectorFileFormat::GeoJson, "EPSG:4326");
    let rows = collect_rows(Arc::new(src), binding).await;
    assert_eq!(rows.len(), 1);
    let (x, y) = parse_wkb_point(&rows[0].geometry);

    // hard floor: reprojection must move us off the lon/lat island.
    assert!(x.abs() > 1_000.0, "expected projected x far from 10°; got x={x:.3}");
    assert!(y.abs() > 1_000.0, "expected projected y far from 56°; got y={y:.3}");
    // soft sanity: UTM zone 32N puts (10°, 56°) near (552000, 6209000).
    assert!(
        (480_000.0..620_000.0).contains(&x),
        "x={x:.3} outside expected UTM 32N easting range"
    );
    assert!(
        (6_100_000.0..6_350_000.0).contains(&y),
        "y={y:.3} outside expected UTM 32N northing range"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unsupported_scheme_yields_backend_error_with_cause() {
    let (src, _tmp) = source_with_temp_cache("EPSG:25832").await;
    // construct a binding with a scheme the adapter's fetcher doesn't know.
    // fragment::parse will still succeed because `.fgb` extension is
    // recognised; the failure surfaces at fetch_bytes.
    let binding = mars_source::SourceBinding::new(
        mars_types::SourceCollectionId::new("weird"),
        "weird://example.invalid/x.fgb#format=flat_geobuf&source_crs=EPSG:25832",
        "geom",
        "id",
        vec![],
        mars_types::CrsCode::new("EPSG:25832"),
    )
    .expect("binding");
    let err = match src.stream_rows(&binding).await {
        Ok(_) => panic!("expected weird scheme to fail"),
        Err(e) => e,
    };
    match &err {
        SourceError::Backend { what, .. } => assert_eq!(*what, "fetch"),
        other => panic!("expected Backend{{what:\"fetch\"}}, got {other:?}"),
    }
    let cause = std::error::Error::source(&err)
        .map(std::string::ToString::to_string)
        .unwrap_or_default();
    assert!(
        cause.contains("weird") || cause.contains("Unsupported"),
        "cause: {cause}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn head_404_yields_backend_error_with_cause() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/missing.geojson"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let (src, _tmp) = source_with_temp_cache("EPSG:25832").await;
    let binding = binding_for(&server, "/missing.geojson", VectorFileFormat::GeoJson, "EPSG:25832");
    let err = match src.stream_rows(&binding).await {
        Ok(_) => panic!("expected 404 to fail"),
        Err(e) => e,
    };
    assert!(matches!(err, SourceError::Backend { what, .. } if what == "fetch"));
}
