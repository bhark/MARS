#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use super::*;

fn solid_tile(w: u32, h: u32, r: u8, g: u8, b: u8, a: u8) -> Arc<DecodedImage> {
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..(w * h) {
        rgba.extend_from_slice(&[r, g, b, a]);
    }
    Arc::new(DecodedImage {
        width: w,
        height: h,
        rgba: Arc::new(rgba),
    })
}

#[test]
fn solid_red_tile_paints_red_pixels_at_dst() {
    let mut pm = Pixmap::new(8, 8).unwrap();
    let tile = solid_tile(2, 2, 255, 0, 0, 255);
    draw(
        &mut pm,
        &tile,
        PixelRect {
            x: 0.0,
            y: 0.0,
            w: 8.0,
            h: 8.0,
        },
        1.0,
        None,
    )
    .unwrap();
    // every pixel of the 8x8 canvas should be opaque red after the blit
    // (a 2x2 red tile scaled to 8x8 with source-over against transparent
    // black). bilinear at the edges still leaves >=99% red at the centre.
    let centre = pm.pixel(4, 4).unwrap();
    assert!(
        centre.red() > 250 && centre.green() < 10 && centre.blue() < 10 && centre.alpha() == 255,
        "centre pixel should be opaque red, got {centre:?}"
    );
}

#[test]
fn opacity_half_attenuates_alpha() {
    let mut pm = Pixmap::new(4, 4).unwrap();
    let tile = solid_tile(1, 1, 255, 0, 0, 255);
    draw(
        &mut pm,
        &tile,
        PixelRect {
            x: 0.0,
            y: 0.0,
            w: 4.0,
            h: 4.0,
        },
        0.5,
        None,
    )
    .unwrap();
    let p = pm.pixel(2, 2).unwrap();
    // source-over of (red, alpha=0.5) onto (transparent black) gives
    // alpha ~= 128 (within rounding).
    assert!(
        (120..=135).contains(&p.alpha()),
        "expected alpha ~128 with opacity 0.5, got {}",
        p.alpha()
    );
}

#[test]
fn dst_offset_paints_only_inside_rect() {
    let mut pm = Pixmap::new(8, 8).unwrap();
    let tile = solid_tile(2, 2, 0, 255, 0, 255);
    draw(
        &mut pm,
        &tile,
        PixelRect {
            x: 4.0,
            y: 4.0,
            w: 4.0,
            h: 4.0,
        },
        1.0,
        None,
    )
    .unwrap();
    // outside the dst rect: transparent. inside: green.
    let outside = pm.pixel(1, 1).unwrap();
    assert_eq!(outside.alpha(), 0, "outside rect must remain transparent");
    let inside = pm.pixel(6, 6).unwrap();
    assert!(
        inside.green() > 250 && inside.red() < 10 && inside.blue() < 10,
        "inside rect must be green, got {inside:?}"
    );
}

#[test]
fn zero_dimension_dst_is_typed_backend_error() {
    let mut pm = Pixmap::new(8, 8).unwrap();
    let tile = solid_tile(2, 2, 255, 0, 0, 255);
    let err = draw(
        &mut pm,
        &tile,
        PixelRect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 4.0,
        },
        1.0,
        None,
    )
    .expect_err("zero-width dst must error");
    assert!(matches!(err, RenderError::Backend(msg) if msg.contains("non-positive")));
}

#[test]
fn zero_sized_tile_is_typed_backend_error() {
    let mut pm = Pixmap::new(8, 8).unwrap();
    let tile = Arc::new(DecodedImage {
        width: 0,
        height: 4,
        rgba: Arc::new(vec![]),
    });
    let err = draw(
        &mut pm,
        &tile,
        PixelRect {
            x: 0.0,
            y: 0.0,
            w: 4.0,
            h: 4.0,
        },
        1.0,
        None,
    )
    .expect_err("zero-sized tile must error");
    assert!(matches!(err, RenderError::Backend(msg) if msg.contains("zero-sized")));
}
