use super::*;

#[test]
fn content_hash_display_is_hex() {
    let h = ContentHash([0xab; 32]);
    let s = h.to_string();
    assert_eq!(s.len(), 64);
    assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(s, h.to_hex());
}

#[test]
fn image_format_mime() {
    assert_eq!(ImageFormat::Png.mime(), "image/png");
    assert_eq!(ImageFormat::Jpeg.mime(), "image/jpeg");
    assert_eq!(ImageFormat::Webp.mime(), "image/webp");
}

#[test]
fn image_format_from_mime_round_trips() {
    for fmt in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Webp] {
        assert_eq!(ImageFormat::from_mime(fmt.mime()), Some(fmt));
    }
    // jpg alias for jpeg
    assert_eq!(ImageFormat::from_mime("image/jpg"), Some(ImageFormat::Jpeg));
    assert_eq!(ImageFormat::from_mime("image/tiff"), None);
}

#[test]
fn image_format_from_extension_is_case_insensitive() {
    assert_eq!(ImageFormat::from_extension("png"), Some(ImageFormat::Png));
    assert_eq!(ImageFormat::from_extension("PNG"), Some(ImageFormat::Png));
    assert_eq!(ImageFormat::from_extension("jpg"), Some(ImageFormat::Jpeg));
    assert_eq!(ImageFormat::from_extension("jpeg"), Some(ImageFormat::Jpeg));
    assert_eq!(ImageFormat::from_extension("webp"), Some(ImageFormat::Webp));
    assert_eq!(ImageFormat::from_extension("tiff"), None);
}
