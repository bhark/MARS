//! end-to-end coverage of `XyzRasterSource::read_tile` against a real
//! `reqwest::Client` round-trip via wiremock. complements the unit tests
//! on `substitute_locator` / `classify_media_type` by pinning the full
//! status -> content-type -> body path, including the empty-content-type
//! reporting added in 471cff3.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mars_source::{RasterBinding, RasterSource, SourceError};
use mars_source_xyz::XyzRasterSource;
use mars_types::{CrsCode, SourceCollectionId};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// stub bytes; the adapter never decodes the payload.
const SAMPLE_PNG: &[u8] = b"\x89PNG\r\n\x1a\nstub-png-body";
const SAMPLE_JPEG: &[u8] = b"\xff\xd8\xffstub-jpeg-body";

fn binding(server: &MockServer) -> RasterBinding {
    RasterBinding {
        collection: SourceCollectionId::new("xyz_test"),
        locator: format!("{}/{{z}}/{{x}}/{{y}}.png", server.uri()),
        source_crs: CrsCode::new("EPSG:3857"),
        tile_size: 256,
        max_level: 19,
    }
}

fn source() -> XyzRasterSource {
    XyzRasterSource::new(reqwest::Client::new())
}

fn cause_string(err: &SourceError) -> String {
    std::error::Error::source(err)
        .map(std::string::ToString::to_string)
        .unwrap_or_default()
}

#[tokio::test]
async fn read_tile_returns_png_bytes_and_media_type() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/13/4069/2707.png"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SAMPLE_PNG.to_vec(), "image/png"))
        .mount(&server)
        .await;

    let tile = source()
        .read_tile(&binding(&server), 13, 4069, 2707)
        .await
        .expect("png read");
    assert_eq!(tile.content_type, "image/png");
    assert_eq!(tile.bytes.as_ref(), SAMPLE_PNG);
}

#[tokio::test]
async fn read_tile_strips_content_type_parameters() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/0/0/0.png"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SAMPLE_JPEG.to_vec(), "image/jpeg; charset=binary"))
        .mount(&server)
        .await;

    let tile = source().read_tile(&binding(&server), 0, 0, 0).await.expect("jpeg read");
    assert_eq!(tile.content_type, "image/jpeg");
    assert_eq!(tile.bytes.as_ref(), SAMPLE_JPEG);
}

#[tokio::test]
async fn read_tile_substitutes_z_x_y_in_request_path() {
    // the mock matches only the exact substituted path. if substitution
    // were broken (e.g. literal "{z}" in the request), no mock would match
    // and wiremock would respond 404 -> TileAbsent, failing the assertion.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/9/123/456.png"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SAMPLE_PNG.to_vec(), "image/png"))
        .mount(&server)
        .await;

    let tile = source()
        .read_tile(&binding(&server), 9, 123, 456)
        .await
        .expect("template substitution must produce /9/123/456.png");
    assert_eq!(tile.content_type, "image/png");
}

#[tokio::test]
async fn read_tile_maps_404_to_tile_absent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let err = source().read_tile(&binding(&server), 5, 1, 2).await.expect_err("404");
    assert!(matches!(err, SourceError::TileAbsent { z: 5, x: 1, y: 2 }));
}

#[tokio::test]
async fn read_tile_maps_204_to_tile_absent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let err = source().read_tile(&binding(&server), 7, 3, 4).await.expect_err("204");
    assert!(matches!(err, SourceError::TileAbsent { z: 7, x: 3, y: 4 }));
}

#[tokio::test]
async fn read_tile_maps_5xx_to_backend_http_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let err = source().read_tile(&binding(&server), 1, 0, 0).await.expect_err("500");
    match err {
        SourceError::Backend { what, .. } => assert_eq!(what, "xyz.tile.http_status"),
        other => panic!("expected Backend, got {other:?}"),
    }
}

#[tokio::test]
async fn read_tile_reports_missing_content_type() {
    // ResponseTemplate::new(200) with no body sets no Content-Type header;
    // exercises the `headers().get(...)` -> None branch.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let err = source()
        .read_tile(&binding(&server), 1, 0, 0)
        .await
        .expect_err("missing CT");
    match &err {
        SourceError::Backend { what, .. } => assert_eq!(*what, "xyz.tile.content_type"),
        other => panic!("expected Backend, got {other:?}"),
    }
    let cause = cause_string(&err);
    assert!(
        cause.contains("missing or non-ascii"),
        "expected missing/non-ascii cause, got: {cause}"
    );
}

#[tokio::test]
async fn read_tile_reports_empty_main_type_distinctly() {
    // pins the 471cff3 fix: a header that's present-but-trims-to-empty
    // after stripping parameters must surface the "empty Content-Type"
    // cause rather than the "missing or non-ascii" cause. a literal
    // `Content-Type: ` is dropped by hyper before it reaches the client,
    // so we exercise the branch via `; charset=binary` (no main type).
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SAMPLE_PNG.to_vec(), "; charset=binary"))
        .mount(&server)
        .await;

    let err = source()
        .read_tile(&binding(&server), 1, 0, 0)
        .await
        .expect_err("empty main type");
    match &err {
        SourceError::Backend { what, .. } => assert_eq!(*what, "xyz.tile.content_type"),
        other => panic!("expected Backend, got {other:?}"),
    }
    let cause = cause_string(&err);
    assert!(
        cause.contains("empty Content-Type"),
        "expected empty-Content-Type cause, got: {cause}"
    );
}

#[tokio::test]
async fn read_tile_reports_unsupported_content_type_with_value() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"<html></html>".to_vec(), "text/html"))
        .mount(&server)
        .await;

    let err = source()
        .read_tile(&binding(&server), 1, 0, 0)
        .await
        .expect_err("text/html");
    let cause = cause_string(&err);
    assert!(
        cause.contains("text/html"),
        "cause should mention text/html, got: {cause}"
    );
}
