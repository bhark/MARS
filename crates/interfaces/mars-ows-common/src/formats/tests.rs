use super::*;

#[test]
fn empty_input_returns_fallback() {
    let out = configured_formats(&[], ImageFormat::Png);
    assert_eq!(out, vec![ImageFormat::Png]);
}

#[test]
fn only_unknown_mimes_returns_fallback() {
    let out = configured_formats(&["application/xyz".into()], ImageFormat::Png);
    assert_eq!(out, vec![ImageFormat::Png]);
}

#[test]
fn parses_known_mimes_in_order() {
    let out = configured_formats(
        &["image/jpeg".into(), "image/png".into(), "unknown".into()],
        ImageFormat::Png,
    );
    assert_eq!(out, vec![ImageFormat::Jpeg, ImageFormat::Png]);
}
