#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn substitute_locator_replaces_all_three_placeholders() {
    let s = substitute_locator("https://t/{z}/{x}/{y}.png", 6, 33, 22).unwrap();
    assert_eq!(s, "https://t/6/33/22.png");
}

#[test]
fn substitute_locator_preserves_query_strings() {
    let s = substitute_locator("https://t/{z}/{x}/{y}.png?key=abc", 0, 1, 2).unwrap();
    assert_eq!(s, "https://t/0/1/2.png?key=abc");
}

#[test]
fn substitute_locator_rejects_missing_placeholder() {
    let err = substitute_locator("https://t/{z}/{x}.png", 0, 0, 0).expect_err("missing {y}");
    assert!(matches!(err, SourceError::InvalidBinding(ref m) if m.contains("{y}")));
}

#[test]
fn classify_media_type_accepts_png_and_jpeg() {
    assert_eq!(classify_media_type("image/png").unwrap(), "image/png");
    assert_eq!(classify_media_type("image/jpeg").unwrap(), "image/jpeg");
    assert_eq!(classify_media_type("image/jpg").unwrap(), "image/jpeg");
}

#[test]
fn classify_media_type_strips_parameters() {
    assert_eq!(classify_media_type("image/png; charset=binary").unwrap(), "image/png");
    assert_eq!(classify_media_type("  image/jpeg ; q=0.8 ").unwrap(), "image/jpeg");
}

#[test]
fn classify_media_type_reports_empty_header_distinctly() {
    let err = classify_media_type("").expect_err("empty");
    let msg = err.to_string();
    // SourceError::Backend stringifies as the static label; the cause walks
    // through Display. assert on both.
    assert!(msg.contains("xyz.tile.content_type"), "got: {msg}");
    let cause = std::error::Error::source(&err).unwrap().to_string();
    assert!(cause.contains("empty Content-Type"), "cause: {cause}");
}

#[test]
fn classify_media_type_reports_unsupported_with_value() {
    let err = classify_media_type("text/html").expect_err("html");
    let cause = std::error::Error::source(&err).unwrap().to_string();
    assert!(cause.contains("text/html"), "cause: {cause}");
}

/// Asserts the adapter remains thread-safe / object-safe per the port
/// contract (`Send + Sync + 'static`). Sole compile-time test, no runtime
/// assertion needed beyond the trait-object construction.
#[test]
fn xyz_raster_source_implements_raster_source_trait_object() {
    fn assert_obj<T: RasterSource + ?Sized>(_: &T) {}
    let src = XyzRasterSource::new(reqwest::Client::new());
    let boxed: Box<dyn RasterSource> = Box::new(src);
    assert_obj(&*boxed);
}
