//! `DrawOp` dispatch hub. exhaustive match on the port-level enum so that
//! adding a variant in `mars-render-port` breaks the build here, forcing the
//! implementation rather than silently falling through.

mod label;
mod path;

use mars_render_port::{DrawOp, RenderError};
use mars_text::Fonts;
use tiny_skia::Pixmap;

pub(crate) fn dispatch(pm: &mut Pixmap, op: &DrawOp, fonts: &Fonts) -> Result<(), RenderError> {
    match op {
        DrawOp::Path { path, style } => path::draw(pm, path, style),
        DrawOp::Label {
            anchor,
            text,
            style,
            angle_rad,
        } => label::draw(pm, *anchor, text, style, *angle_rad, fonts),
        // staged variants: runtime may emit them, adapter has not wired the
        // pipeline yet. typed error keeps the contract honest instead of a
        // silent debug log.
        DrawOp::Symbol { .. } => Err(RenderError::NotImplemented { what: "DrawOp::Symbol" }),
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
    use mars_style::Style;

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
    fn symbol_variant_returns_not_implemented() {
        let op = DrawOp::Symbol {
            anchor: (8.0, 8.0),
            rotation_rad: 0.0,
            style: Arc::new(Style::default()),
        };
        let err = renderer().render(canvas(), &[op]).expect_err("must error");
        assert!(matches!(err, RenderError::NotImplemented { what } if what == "DrawOp::Symbol"));
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
}
