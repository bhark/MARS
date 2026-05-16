//! Per-`DrawOp`-variant tiny-skia draw helpers. The port-level
//! [`mars_render_port::dispatch_ops`] walks `DrawOp`s and routes each
//! through the [`mars_render_port::Surface`] impl in `crate::surface`,
//! which in turn calls into these helpers. Adding a new `DrawOp` variant
//! breaks the build at the Surface impl, which is the canonical seam.

pub(crate) mod label;
pub(crate) mod path;
pub(crate) mod pattern;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use mars_render_port::{Canvas, DrawOp, Path as PortPath, RenderError, Renderer, Subpath};
    use mars_style::{MarkerShape, MarkerSymbol, Style};

    use crate::TinySkiaRenderer;

    fn renderer() -> TinySkiaRenderer {
        TinySkiaRenderer::new(Arc::new(mars_text::Fonts::with_default()))
    }

    fn canvas() -> Canvas {
        Canvas {
            width: 16,
            height: 16,
            background: None,
        }
    }

    #[test]
    fn symbol_circle_dispatches_to_renderer() {
        let op = DrawOp::Symbol {
            anchor: (8.0, 8.0),
            rotation_rad: 0.0,
            style: Arc::new(
                Style {
                    fill: Some(mars_style::FillPaint::Solid(mars_style::Colour {
                        r: 255,
                        g: 0,
                        b: 0,
                        a: 255,
                    })),
                    marker: Some(MarkerSymbol {
                        shape: MarkerShape::Circle,
                        size: 6.0.into(),
                        angle: None,
                    }),
                    ..Default::default()
                }
                .resolve(0),
            ),
        };
        // smoke assertion at the DrawOp seam: per-variant pixel coverage
        // is verified inside symbol::tests; here we only confirm the arm
        // routes through dispatch and produces a pixmap.
        let _ = renderer().render(canvas(), &[op]).expect("render ok");
    }

    #[test]
    fn pattern_image_with_empty_registry_returns_image_not_found() {
        let op = DrawOp::Pattern {
            path: PortPath {
                subpaths: vec![Subpath {
                    points: vec![(2.0, 2.0), (12.0, 2.0), (12.0, 12.0), (2.0, 12.0)],
                    closed: true,
                }],
            },
            style: Arc::new(
                Style {
                    fill: Some(mars_style::FillPaint::Image { name: "brick".into() }),
                    ..Default::default()
                }
                .resolve(0),
            ),
        };
        let err = renderer().render(canvas(), &[op]).expect_err("missing image");
        assert!(matches!(err, RenderError::ImageNotFound { ref name } if name == "brick"));
    }

    #[test]
    fn pattern_with_solid_fill_returns_routing_contract_error() {
        // procedural fill emitted via DrawOp::Pattern is a runtime/renderer
        // contract slip; the typed Backend error pinpoints the seam.
        let op = DrawOp::Pattern {
            path: PortPath {
                subpaths: vec![Subpath {
                    points: vec![(2.0, 2.0), (12.0, 2.0), (12.0, 12.0), (2.0, 12.0)],
                    closed: true,
                }],
            },
            style: Arc::new(
                Style {
                    fill: Some(mars_style::FillPaint::Solid(mars_style::Colour {
                        r: 255,
                        g: 0,
                        b: 0,
                        a: 255,
                    })),
                    ..Default::default()
                }
                .resolve(0),
            ),
        };
        let err = renderer().render(canvas(), &[op]).expect_err("routing error");
        assert!(matches!(err, RenderError::Backend(msg) if msg.contains("DrawOp::Path")));
    }

    #[test]
    fn raster_op_paints_tile_pixels_into_canvas() {
        use mars_render_port::{DecodedImage, PixelRect};
        // 1x1 opaque red tile blown up to fill the entire 16x16 canvas.
        let tile = Arc::new(DecodedImage {
            width: 1,
            height: 1,
            rgba: Arc::new(vec![255, 0, 0, 255]),
        });
        let op = DrawOp::Raster {
            tile,
            dst: PixelRect {
                x: 0.0,
                y: 0.0,
                w: 16.0,
                h: 16.0,
            },
            opacity: 1.0,
            blend_mode: None,
        };
        let pm = renderer().render(canvas(), &[op]).expect("raster paints");
        // rendered output is premultiplied RGBA. opaque red premultiplies to
        // itself (255,0,0,255), so every pixel should match exactly.
        let red_count = pm
            .premultiplied_rgba
            .chunks_exact(4)
            .filter(|p| p[0] > 250 && p[1] < 10 && p[2] < 10 && p[3] == 255)
            .count();
        assert_eq!(red_count, 16 * 16, "every pixel should be opaque red");
    }

    #[test]
    fn symbol_with_glyph_marker_dispatches_through_glyph_path() {
        let op = DrawOp::Symbol {
            anchor: (16.0, 16.0),
            rotation_rad: 0.0,
            style: Arc::new(
                Style {
                    fill: Some(mars_style::FillPaint::Solid(mars_style::Colour::rgba(255, 0, 0, 255))),
                    marker: Some(MarkerSymbol {
                        shape: MarkerShape::Glyph {
                            font_family: "DejaVu Sans".into(),
                            ch: "A".into(),
                        },
                        size: 18.0.into(),
                        angle: None,
                    }),
                    ..Default::default()
                }
                .resolve(0),
            ),
        };
        let canvas = Canvas {
            width: 32,
            height: 32,
            background: None,
        };
        let pm = renderer().render(canvas, &[op]).expect("render ok");
        // verify pixels actually moved - at least one painted alpha byte.
        assert!(
            pm.premultiplied_rgba.chunks_exact(4).any(|p| p[3] > 0),
            "glyph marker render must paint at least one pixel"
        );
    }
}
