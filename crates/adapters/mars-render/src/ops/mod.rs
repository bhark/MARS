//! `DrawOp` dispatch hub. exhaustive match on the port-level enum so that
//! adding a variant in `mars-render-port` breaks the build here, forcing the
//! implementation rather than silently falling through.

mod label;
pub(crate) mod path;
mod pattern;

use mars_render_port::{DrawOp, RenderError};
use mars_text::Fonts;
use tiny_skia::Pixmap;

use crate::prepare::UnimplementedFeatures;
use crate::symbol;

pub(crate) fn dispatch(pm: &mut Pixmap, op: &DrawOp, fonts: &Fonts) -> Result<UnimplementedFeatures, RenderError> {
    match op {
        DrawOp::Path { path, style } => path::draw(pm, path, style),
        DrawOp::Label {
            anchor,
            text,
            style,
            angle_rad,
        } => label::draw(pm, *anchor, text, style, *angle_rad, fonts),
        DrawOp::Symbol {
            anchor,
            rotation_rad,
            style,
        } => symbol::dispatch(pm, *anchor, *rotation_rad, style),
        DrawOp::Pattern { path, style } => pattern::draw(pm, path, style),
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
    fn pattern_image_routes_to_pattern_dispatch() {
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
        let err = renderer().render(canvas(), &[op]).expect_err("image stub");
        assert!(matches!(err, RenderError::NotImplemented { what } if what == "FillPaint::Image"));
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
    fn symbol_with_glyph_marker_surfaces_unimplemented_flag() {
        use tiny_skia::Pixmap as SkPixmap;

        let mut pm = SkPixmap::new(16, 16).unwrap();
        let fonts = mars_text::Fonts::with_default();
        let op = DrawOp::Symbol {
            anchor: (8.0, 8.0),
            rotation_rad: 0.0,
            style: Arc::new(Style {
                marker: Some(MarkerSymbol::Glyph {
                    font_family: "x".into(),
                    ch: "a".into(),
                    size: 6.0,
                }),
                ..Default::default()
            }),
        };
        let flags = dispatch(&mut pm, &op, &fonts).expect("dispatch ok");
        assert!(
            flags.glyph_marker,
            "glyph_marker flag must propagate from symbol dispatch"
        );
    }

    #[test]
    fn path_with_glyph_marker_surfaces_unimplemented_flag() {
        use tiny_skia::Pixmap as SkPixmap;

        let mut pm = SkPixmap::new(16, 16).unwrap();
        let fonts = mars_text::Fonts::with_default();
        let op = DrawOp::Path {
            path: PortPath {
                subpaths: vec![Subpath {
                    points: vec![(2.0, 2.0), (12.0, 12.0)],
                    closed: false,
                }],
            },
            style: Arc::new(Style {
                marker: Some(MarkerSymbol::Glyph {
                    font_family: "x".into(),
                    ch: "a".into(),
                    size: 6.0,
                }),
                ..Default::default()
            }),
        };
        let flags = dispatch(&mut pm, &op, &fonts).expect("dispatch ok");
        assert!(
            flags.glyph_marker,
            "glyph_marker flag must propagate from path dispatch"
        );
    }
}
