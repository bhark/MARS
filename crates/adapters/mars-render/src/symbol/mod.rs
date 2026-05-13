//! point-marker dispatch hub. mirrors the variant-per-file shape of
//! `fill/` and `stroke/`: each `MarkerSymbol` variant lives in a sibling
//! module and is reached through a single exhaustive match. adding a
//! variant in `mars-style` breaks the build here, forcing the conversation
//! about whether the new marker is wired or staged.
//!
//! `Glyph` continues to surface through `UnimplementedFeatures::glyph_marker`
//! (the existing flag); the warn-but-continue contract in the renderer
//! entry point already covers it. A `None` marker on a `Symbol` op is a
//! runtime contract slip; rendering no-ops rather than aborting the batch,
//! consistent with how empty paths and zero-width strokes are tolerated
//! elsewhere in the pipeline.

mod circle;
mod cross;
mod pin;
mod square;
mod triangle;
mod vector_shape;
mod x;

use mars_render_port::RenderError;
use mars_style::{MarkerSymbol, Style};
use tiny_skia::Pixmap;

use crate::prepare::UnimplementedFeatures;

pub(crate) fn dispatch(
    pm: &mut Pixmap,
    anchor: (f32, f32),
    rotation_rad: f32,
    style: &Style,
) -> Result<UnimplementedFeatures, RenderError> {
    let _ = pm;
    let _ = anchor;
    let _ = rotation_rad;
    let Some(marker) = &style.marker else {
        return Ok(UnimplementedFeatures::default());
    };
    match marker {
        MarkerSymbol::Glyph { .. } => Ok(UnimplementedFeatures {
            glyph_marker: true,
            ..Default::default()
        }),
        MarkerSymbol::Circle { .. } => Err(RenderError::NotImplemented {
            what: "MarkerSymbol::Circle",
        }),
        MarkerSymbol::Square { .. } => Err(RenderError::NotImplemented {
            what: "MarkerSymbol::Square",
        }),
        MarkerSymbol::Triangle { .. } => Err(RenderError::NotImplemented {
            what: "MarkerSymbol::Triangle",
        }),
        MarkerSymbol::Cross { .. } => Err(RenderError::NotImplemented {
            what: "MarkerSymbol::Cross",
        }),
        MarkerSymbol::X { .. } => Err(RenderError::NotImplemented {
            what: "MarkerSymbol::X",
        }),
        MarkerSymbol::Pin { .. } => Err(RenderError::NotImplemented {
            what: "MarkerSymbol::Pin",
        }),
        MarkerSymbol::VectorShape { .. } => Err(RenderError::NotImplemented {
            what: "MarkerSymbol::VectorShape",
        }),
        // `#[non_exhaustive]` forward-compat: future variants land here
        // until they grow a sibling module + dispatch arm above.
        _ => Err(RenderError::NotImplemented {
            what: "unknown MarkerSymbol variant",
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use mars_style::Style;
    use tiny_skia::Pixmap as SkPixmap;

    fn pm() -> SkPixmap {
        SkPixmap::new(16, 16).unwrap()
    }

    #[test]
    fn none_marker_is_silent_no_op() {
        let style = Style::default();
        let flags = dispatch(&mut pm(), (8.0, 8.0), 0.0, &style).expect("ok");
        assert!(!flags.any(), "no-op must not flag");
    }

    #[test]
    fn glyph_marker_surfaces_flag_without_error() {
        let style = Style {
            marker: Some(MarkerSymbol::Glyph {
                font_family: "x".into(),
                ch: "a".into(),
                size: 6.0,
            }),
            ..Default::default()
        };
        let flags = dispatch(&mut pm(), (8.0, 8.0), 0.0, &style).expect("ok");
        assert!(flags.glyph_marker, "glyph_marker must propagate");
    }

    #[test]
    fn circle_marker_returns_typed_not_implemented() {
        let style = Style {
            marker: Some(MarkerSymbol::Circle { size: 6.0 }),
            ..Default::default()
        };
        let err = dispatch(&mut pm(), (8.0, 8.0), 0.0, &style).expect_err("must error");
        assert!(matches!(err, RenderError::NotImplemented { what } if what == "MarkerSymbol::Circle"));
    }
}
