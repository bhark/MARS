//! `DrawOp` dispatch hub. exhaustive match on the port-level enum so that
//! adding a variant in `mars-render-port` breaks the build here, forcing the
//! implementation rather than silently falling through.

mod label;
pub(crate) mod path;
mod pattern;

use mars_render_port::{DrawOp, ImageRegistry, RenderError};
use mars_text::Fonts;
use tiny_skia::Pixmap;

use crate::prepare::UnimplementedFeatures;
use crate::{raster, symbol};

pub(crate) fn dispatch(
    pm: &mut Pixmap,
    op: &DrawOp,
    fonts: &Fonts,
    images: &dyn ImageRegistry,
) -> Result<UnimplementedFeatures, RenderError> {
    match op {
        DrawOp::Path { path, style } => path::draw(pm, path, style),
        DrawOp::Label {
            anchor,
            text,
            style,
            angle_rad,
        } => label::draw(pm, *anchor, text, style, *angle_rad, fonts),
        DrawOp::FollowLabel {
            polyline_px,
            start_arc_px,
            text,
            style,
        } => label::draw_follow(pm, polyline_px, *start_arc_px, text, style, fonts),
        DrawOp::Symbol {
            anchor,
            rotation_rad,
            style,
        } => symbol::dispatch(pm, *anchor, *rotation_rad, style, fonts),
        DrawOp::Pattern { path, style } => pattern::draw(pm, path, style, images),
        DrawOp::Raster { tile, dst, opacity } => {
            raster::draw(pm, tile, *dst, *opacity).map(|()| UnimplementedFeatures::default())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use mars_render_port::{Canvas, DrawOp, Path as PortPath, RenderError, Renderer, Subpath};
    use mars_style::{MarkerSymbol, Style};

    use super::dispatch;
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
            style: Arc::new(Style {
                fill: Some(mars_style::FillPaint::Solid(mars_style::Colour {
                    r: 255,
                    g: 0,
                    b: 0,
                    a: 255,
                })),
                marker: Some(MarkerSymbol::Circle { size: 6.0 }),
                ..Default::default()
            }),
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
            style: Arc::new(Style {
                fill: Some(mars_style::FillPaint::Image { name: "brick".into() }),
                ..Default::default()
            }),
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
            style: Arc::new(Style {
                fill: Some(mars_style::FillPaint::Solid(mars_style::Colour {
                    r: 255,
                    g: 0,
                    b: 0,
                    a: 255,
                })),
                ..Default::default()
            }),
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
        use tiny_skia::Pixmap as SkPixmap;

        let mut pm = SkPixmap::new(32, 32).unwrap();
        let fonts = mars_text::Fonts::with_default();
        let op = DrawOp::Symbol {
            anchor: (16.0, 16.0),
            rotation_rad: 0.0,
            style: Arc::new(Style {
                fill: Some(mars_style::FillPaint::Solid(mars_style::Colour::rgba(255, 0, 0, 255))),
                marker: Some(MarkerSymbol::Glyph {
                    font_family: "DejaVu Sans".into(),
                    ch: "A".into(),
                    size: 18.0,
                }),
                ..Default::default()
            }),
        };
        let flags = dispatch(&mut pm, &op, &fonts, &mars_render_port::EmptyImageRegistry).expect("dispatch ok");
        assert!(
            !flags.any(),
            "glyph implementation must not surface unimplemented flags"
        );
        // verify pixels actually moved - at least one painted alpha byte.
        assert!(
            pm.data().chunks_exact(4).any(|p| p[3] > 0),
            "glyph dispatch must paint at least one pixel"
        );
    }
}
