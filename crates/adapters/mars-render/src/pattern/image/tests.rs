#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use mars_render_port::{DecodedImage, EmptyImageRegistry};
use tiny_skia::{PathBuilder, Pixmap as SkPixmap};

use super::*;

fn square_path() -> tiny_skia::Path {
    let mut pb = PathBuilder::new();
    pb.move_to(2.0, 2.0);
    pb.line_to(14.0, 2.0);
    pb.line_to(14.0, 14.0);
    pb.line_to(2.0, 14.0);
    pb.close();
    pb.finish().unwrap()
}

// 2x2 RGBA checker: opaque red and transparent.
#[derive(Debug)]
struct CheckerRegistry;
impl ImageRegistry for CheckerRegistry {
    fn get(&self, name: &str) -> Option<Arc<DecodedImage>> {
        if name != "checker" {
            return None;
        }
        Some(Arc::new(DecodedImage {
            width: 2,
            height: 2,
            rgba: Arc::new(vec![
                255, 0, 0, 255, 0, 0, 0, 0, // row 0: red, transparent
                0, 0, 0, 0, 255, 0, 0, 255, // row 1: transparent, red
            ]),
        }))
    }
}

#[test]
fn missing_name_returns_typed_image_not_found() {
    let mut pm = SkPixmap::new(16, 16).unwrap();
    let err = draw(
        &mut pm,
        &square_path(),
        "brick",
        1.0,
        BlendMode::SourceOver,
        &EmptyImageRegistry,
    )
    .expect_err("missing must error");
    assert!(matches!(err, RenderError::ImageNotFound { ref name } if name == "brick"));
}

#[test]
fn checker_pattern_fills_with_red_tiled() {
    let mut pm = SkPixmap::new(16, 16).unwrap();
    draw(
        &mut pm,
        &square_path(),
        "checker",
        1.0,
        BlendMode::SourceOver,
        &CheckerRegistry,
    )
    .expect("fill ok");
    // tile is 2x2, half red, half transparent. expect roughly half the
    // filled-square's interior to carry red coverage. the 12x12 filled
    // square covers ~144 pixels; tile alternates so ~50% red.
    let red_count = pm
        .pixels()
        .iter()
        .filter(|p| p.red() > 200 && p.green() < 40 && p.blue() < 40 && p.alpha() == 255)
        .count();
    assert!(
        red_count > 50 && red_count < 100,
        "expected ~72 red pixels, got {red_count}"
    );
}

#[test]
fn invalid_rgba_length_surfaces_backend_error() {
    #[derive(Debug)]
    struct BadRegistry;
    impl ImageRegistry for BadRegistry {
        fn get(&self, _: &str) -> Option<Arc<DecodedImage>> {
            Some(Arc::new(DecodedImage {
                width: 2,
                height: 2,
                rgba: Arc::new(vec![0u8; 7]), // wrong size
            }))
        }
    }
    let mut pm = SkPixmap::new(8, 8).unwrap();
    let err = draw(&mut pm, &square_path(), "x", 1.0, BlendMode::SourceOver, &BadRegistry).expect_err("must error");
    assert!(matches!(err, RenderError::Backend(msg) if msg.contains("rgba length")));
}
