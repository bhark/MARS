#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn encode_png_rgba(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().unwrap();
    writer.write_image_data(rgba).unwrap();
    drop(writer);
    out
}

#[test]
fn decode_png_roundtrips_2x2_rgba() {
    let rgba = vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 128];
    let bytes = encode_png_rgba(2, 2, &rgba);
    let decoded = decode_png_to_rgba(&bytes).unwrap();
    assert_eq!(decoded.width, 2);
    assert_eq!(decoded.height, 2);
    assert_eq!(decoded.rgba.as_slice(), rgba.as_slice());
}

#[test]
fn decode_to_rgba_routes_by_content_type() {
    let rgba = vec![10, 20, 30, 255];
    let png_bytes = encode_png_rgba(1, 1, &rgba);
    let d = decode_to_rgba(&png_bytes, "image/png").unwrap();
    assert_eq!(d.rgba.as_slice(), rgba.as_slice());
    let err = decode_to_rgba(&png_bytes, "image/webp").expect_err("unsupported");
    assert!(err.contains("unsupported"));
}
