//! style normalisation: condense an Option-heavy `Style` into a non-Option,
//! validated view that drives the fill and stroke pipelines.
//!
//! adding a new `Style` field touches `resolve` in one place; downstream code
//! reads from `Resolved*` instead of re-doing the Option dance per call site.

use mars_style::{FillPaint, MarkerSymbol, Style};
use tiny_skia::{LineCap, LineJoin, StrokeDash};

use crate::canvas::{map_cap, map_join};
use crate::stroke;

#[derive(Debug)]
pub(crate) struct Resolved {
    pub fill: Option<ResolvedFill>,
    pub stroke: Option<ResolvedStroke>,
    /// style fields the renderer accepts in the wire form but does not yet
    /// honour. populated by `resolve`; the dispatch hub warns once per render
    /// so partial-render silent-drops are visible without aborting. when a
    /// strict-mode hook lands, this is the single chokepoint.
    pub unimplemented: UnimplementedFeatures,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct UnimplementedFeatures {
    pub glyph_marker: bool,
    pub stroke_gap: bool,
}

impl UnimplementedFeatures {
    pub(crate) fn any(self) -> bool {
        self.glyph_marker || self.stroke_gap
    }

    pub(crate) fn names(self) -> impl Iterator<Item = &'static str> {
        [
            self.glyph_marker.then_some("Style::marker (Glyph)"),
            self.stroke_gap.then_some("Style::stroke_gap"),
        ]
        .into_iter()
        .flatten()
    }

    pub(crate) fn merge(&mut self, other: Self) {
        self.glyph_marker |= other.glyph_marker;
        self.stroke_gap |= other.stroke_gap;
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ResolvedFill {
    pub paint: FillPaint,
    /// 0..=1; style.opacity baked in. multiplied into the paint colour's
    /// alpha by the fill pipeline.
    pub alpha: f32,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedStroke {
    pub colour: mars_style::Colour,
    /// 0..=1; style.opacity and the sub-pixel-stroke alpha-scale (AGG-style
    /// emulation: widths <1 paint a 1px stroke at proportional alpha) folded
    /// together.
    pub alpha: f32,
    /// final tiny-skia stroke width: clamped up to 1.0 if the requested
    /// width was a sub-pixel value (the alpha-scale carries the visual
    /// intensity instead).
    pub width: f32,
    pub cap: LineCap,
    pub join: LineJoin,
    pub dash: Option<StrokeDash>,
    /// 0 if unset; positive = right of direction of travel in y-down pixel
    /// space.
    pub offset_px: f32,
}

pub(crate) fn resolve(style: &Style) -> Resolved {
    let opacity = style.opacity.unwrap_or(1.0).clamp(0.0, 1.0);

    let unimplemented = UnimplementedFeatures {
        glyph_marker: matches!(style.marker, Some(MarkerSymbol::Glyph { .. })),
        stroke_gap: style.stroke_gap.is_some(),
    };

    let fill = style.fill.map(|paint| ResolvedFill { paint, alpha: opacity });

    let stroke = style.stroke.and_then(|colour| {
        let requested = style.stroke_width.unwrap_or(1.0);
        if !(requested.is_finite() && requested > 0.0) {
            return None;
        }
        let (width, alpha_scale) = if requested < 1.0 {
            (1.0, requested)
        } else {
            (requested, 1.0)
        };
        let dash = style.stroke_dasharray.as_deref().and_then(stroke::dash::build);
        let offset_px = match style.stroke_offset_px {
            Some(d) if d.is_finite() && d.abs() > f32::EPSILON => d,
            _ => 0.0,
        };
        Some(ResolvedStroke {
            colour,
            alpha: alpha_scale * opacity,
            width,
            cap: style.stroke_linecap.map(map_cap).unwrap_or(LineCap::Butt),
            join: style.stroke_linejoin.map(map_join).unwrap_or(LineJoin::Miter),
            dash,
            offset_px,
        })
    });

    Resolved {
        fill,
        stroke,
        unimplemented,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use mars_style::{Colour, FillPaint, LineCap as SLineCap, LineJoin as SLineJoin, Style};

    #[test]
    fn opacity_is_baked_into_fill_alpha() {
        let s = Style {
            fill: Some(FillPaint::Solid(Colour::rgba(255, 0, 0, 255))),
            opacity: Some(0.5),
            ..Default::default()
        };
        let r = resolve(&s);
        let f = r.fill.expect("fill");
        assert!((f.alpha - 0.5).abs() < 1e-6);
    }

    #[test]
    fn stroke_defaults_to_butt_miter() {
        let s = Style {
            stroke: Some(Colour::rgba(0, 0, 0, 255)),
            stroke_width: Some(2.0),
            ..Default::default()
        };
        let r = resolve(&s);
        let st = r.stroke.expect("stroke");
        assert!(matches!(st.cap, LineCap::Butt));
        assert!(matches!(st.join, LineJoin::Miter));
        assert!((st.alpha - 1.0).abs() < 1e-6);
        assert!((st.width - 2.0).abs() < 1e-6);
        assert!(st.dash.is_none());
        assert_eq!(st.offset_px, 0.0);
    }

    #[test]
    fn subpixel_stroke_clamps_width_and_scales_alpha() {
        // requested width 0.25 + opacity 0.8 -> width 1.0, alpha 0.25*0.8 = 0.2
        let s = Style {
            stroke: Some(Colour::rgba(0, 0, 0, 255)),
            stroke_width: Some(0.25),
            opacity: Some(0.8),
            ..Default::default()
        };
        let st = resolve(&s).stroke.expect("stroke");
        assert!((st.width - 1.0).abs() < 1e-6);
        assert!((st.alpha - 0.2).abs() < 1e-6);
    }

    #[test]
    fn zero_width_stroke_drops() {
        let s = Style {
            stroke: Some(Colour::rgba(0, 0, 0, 255)),
            stroke_width: Some(0.0),
            ..Default::default()
        };
        assert!(resolve(&s).stroke.is_none());
    }

    #[test]
    fn dash_array_passes_through_when_even_length() {
        let s = Style {
            stroke: Some(Colour::rgba(0, 0, 0, 255)),
            stroke_width: Some(2.0),
            stroke_dasharray: Some(vec![4.0, 2.0]),
            ..Default::default()
        };
        let st = resolve(&s).stroke.expect("stroke");
        assert!(st.dash.is_some());
    }

    #[test]
    fn dash_array_odd_length_falls_back_to_solid() {
        let s = Style {
            stroke: Some(Colour::rgba(0, 0, 0, 255)),
            stroke_width: Some(2.0),
            stroke_dasharray: Some(vec![4.0, 2.0, 1.0]),
            ..Default::default()
        };
        let st = resolve(&s).stroke.expect("stroke");
        assert!(st.dash.is_none());
    }

    #[test]
    fn stroke_cap_join_translate() {
        let s = Style {
            stroke: Some(Colour::rgba(0, 0, 0, 255)),
            stroke_width: Some(2.0),
            stroke_linecap: Some(SLineCap::Round),
            stroke_linejoin: Some(SLineJoin::Bevel),
            ..Default::default()
        };
        let st = resolve(&s).stroke.expect("stroke");
        assert!(matches!(st.cap, LineCap::Round));
        assert!(matches!(st.join, LineJoin::Bevel));
    }

    #[test]
    fn unimplemented_flags_glyph_marker() {
        let s = Style {
            marker: Some(MarkerSymbol::Glyph {
                font_family: "x".into(),
                ch: "a".into(),
                size: 6.0,
            }),
            ..Default::default()
        };
        let r = resolve(&s);
        assert!(r.unimplemented.glyph_marker);
        assert!(!r.unimplemented.stroke_gap);
        let names: Vec<_> = r.unimplemented.names().collect();
        assert_eq!(names, vec!["Style::marker (Glyph)"]);
    }

    #[test]
    fn unimplemented_flags_stroke_gap() {
        let s = Style {
            stroke: Some(Colour::rgba(0, 0, 0, 255)),
            stroke_width: Some(1.0),
            stroke_gap: Some(mars_style::StrokeGap {
                interval_px: 12.0,
                initial_px: 0.0,
            }),
            ..Default::default()
        };
        let r = resolve(&s);
        assert!(r.unimplemented.stroke_gap);
        assert!(!r.unimplemented.glyph_marker);
        assert!(r.unimplemented.any());
    }

    #[test]
    fn unimplemented_empty_when_no_unsupported_fields() {
        let s = Style {
            fill: Some(FillPaint::Solid(Colour::rgba(0, 0, 0, 255))),
            ..Default::default()
        };
        let r = resolve(&s);
        assert!(!r.unimplemented.any());
        assert_eq!(r.unimplemented.names().count(), 0);
    }

    #[test]
    fn stroke_offset_zero_when_tiny() {
        let s = Style {
            stroke: Some(Colour::rgba(0, 0, 0, 255)),
            stroke_width: Some(1.0),
            stroke_offset_px: Some(0.0),
            ..Default::default()
        };
        assert_eq!(resolve(&s).stroke.expect("stroke").offset_px, 0.0);
    }
}
