//! `DrawOp` dispatch hub. exhaustive match on the port-level enum so that
//! adding a variant in `mars-render-port` breaks the build here, forcing the
//! implementation rather than silently falling through.

mod label;
mod path;

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
        // pattern fills still stub at the DrawOp level; the slice-2
        // commit wires this through pattern::dispatch.
        DrawOp::Pattern { .. } => Err(RenderError::NotImplemented {
            what: "DrawOp::Pattern",
        }),
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
    fn symbol_circle_returns_typed_not_implemented_until_implemented() {
        // scaffold-only assertion: the circle marker variant has not yet
        // been wired to a build_path implementation; the typed error names
        // the specific MarkerSymbol variant so the next commit can flip
        // this to a positive render assertion at one named site.
        let op = DrawOp::Symbol {
            anchor: (8.0, 8.0),
            rotation_rad: 0.0,
            style: Arc::new(Style {
                marker: Some(MarkerSymbol::Circle { size: 6.0 }),
                ..Default::default()
            }),
        };
        let err = renderer().render(canvas(), &[op]).expect_err("must error");
        assert!(matches!(err, RenderError::NotImplemented { what } if what == "MarkerSymbol::Circle"));
    }

    #[test]
    fn pattern_variant_returns_not_implemented() {
        let op = DrawOp::Pattern {
            path: PortPath {
                subpaths: vec![Subpath {
                    points: vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)],
                    closed: true,
                }],
            },
            style: Arc::new(Style::default()),
        };
        let err = renderer().render(canvas(), &[op]).expect_err("must error");
        assert!(matches!(err, RenderError::NotImplemented { what } if what == "DrawOp::Pattern"));
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
