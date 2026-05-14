//! tiled image pattern fill. Resolves `FillPaint::Image { name }` against
//! the renderer's `ImageRegistry`, builds a tiny-skia tile-pattern shader,
//! and fills the path. Unknown names surface as
//! [`RenderError::ImageNotFound`] so the runtime can distinguish "asset
//! missing" from "feature not implemented".

use mars_render_port::{ImageRegistry, RenderError};
use tiny_skia::{FillRule, FilterQuality, Paint, Pattern as SkPattern, Pixmap, SpreadMode, Transform};

use crate::decoded_image::build_premultiplied;

pub(crate) fn draw(
    pm: &mut Pixmap,
    path: &tiny_skia::Path,
    name: &str,
    alpha: f32,
    images: &dyn ImageRegistry,
) -> Result<(), RenderError> {
    let image = images
        .get(name)
        .ok_or_else(|| RenderError::ImageNotFound { name: name.into() })?;
    let tile = build_premultiplied(&image)?;
    let pattern = SkPattern::new(
        tile.as_ref(),
        SpreadMode::Repeat,
        FilterQuality::Nearest,
        alpha.clamp(0.0, 1.0),
        Transform::identity(),
    );
    let paint = Paint {
        shader: pattern,
        anti_alias: true,
        ..Default::default()
    };
    pm.fill_path(path, &paint, FillRule::EvenOdd, Transform::identity(), None);
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
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
        let err = draw(&mut pm, &square_path(), "brick", 1.0, &EmptyImageRegistry).expect_err("missing must error");
        assert!(matches!(err, RenderError::ImageNotFound { ref name } if name == "brick"));
    }

    #[test]
    fn checker_pattern_fills_with_red_tiled() {
        let mut pm = SkPixmap::new(16, 16).unwrap();
        draw(&mut pm, &square_path(), "checker", 1.0, &CheckerRegistry).expect("fill ok");
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
        let err = draw(&mut pm, &square_path(), "x", 1.0, &BadRegistry).expect_err("must error");
        assert!(matches!(err, RenderError::Backend(msg) if msg.contains("rgba length")));
    }
}
