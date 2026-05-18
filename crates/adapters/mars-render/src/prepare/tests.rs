#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use mars_style::{Colour, FillPaint, LineCap as SLineCap, LineJoin as SLineJoin, Style};

// helper: drive resolve through the same Style->ResolvedStyle seam the
// runtime uses. denom is irrelevant for these tests since the authored
// sizes are bare f32 (no ref_denom), so 0 keeps the values literal.
fn r(s: &Style) -> Resolved {
    resolve(&s.resolve(0))
}

#[test]
fn opacity_is_baked_into_fill_alpha() {
    let s = Style {
        fill: Some(FillPaint::Solid(Colour::rgba(255, 0, 0, 255))),
        opacity: Some(0.5),
        ..Default::default()
    };
    let resolved = r(&s);
    let f = resolved.fill.expect("fill");
    assert!((f.alpha - 0.5).abs() < 1e-6);
}

#[test]
fn stroke_defaults_to_butt_miter() {
    let s = Style {
        stroke: Some(Colour::rgba(0, 0, 0, 255)),
        stroke_width: Some(2.0.into()),
        ..Default::default()
    };
    let resolved = r(&s);
    let st = resolved.stroke.expect("stroke");
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
        stroke_width: Some(0.25.into()),
        opacity: Some(0.8),
        ..Default::default()
    };
    let st = r(&s).stroke.expect("stroke");
    assert!((st.width - 1.0).abs() < 1e-6);
    assert!((st.alpha - 0.2).abs() < 1e-6);
}

#[test]
fn zero_width_stroke_drops() {
    let s = Style {
        stroke: Some(Colour::rgba(0, 0, 0, 255)),
        stroke_width: Some(0.0.into()),
        ..Default::default()
    };
    assert!(r(&s).stroke.is_none());
}

#[test]
fn dash_array_passes_through_when_even_length() {
    let s = Style {
        stroke: Some(Colour::rgba(0, 0, 0, 255)),
        stroke_width: Some(2.0.into()),
        stroke_dasharray: Some(vec![4.0, 2.0]),
        ..Default::default()
    };
    let st = r(&s).stroke.expect("stroke");
    assert!(st.dash.is_some());
}

#[test]
fn dash_array_odd_length_falls_back_to_solid() {
    let s = Style {
        stroke: Some(Colour::rgba(0, 0, 0, 255)),
        stroke_width: Some(2.0.into()),
        stroke_dasharray: Some(vec![4.0, 2.0, 1.0]),
        ..Default::default()
    };
    let st = r(&s).stroke.expect("stroke");
    assert!(st.dash.is_none());
}

#[test]
fn stroke_cap_join_translate() {
    let s = Style {
        stroke: Some(Colour::rgba(0, 0, 0, 255)),
        stroke_width: Some(2.0.into()),
        stroke_linecap: Some(SLineCap::Round),
        stroke_linejoin: Some(SLineJoin::Bevel),
        ..Default::default()
    };
    let st = r(&s).stroke.expect("stroke");
    assert!(matches!(st.cap, LineCap::Round));
    assert!(matches!(st.join, LineJoin::Bevel));
}

#[test]
fn stroke_gap_resolves_when_marker_present() {
    let s = Style {
        stroke: Some(Colour::rgba(0, 0, 0, 255)),
        stroke_width: Some(1.0.into()),
        marker: Some(mars_style::MarkerSymbol {
            shape: mars_style::MarkerShape::Circle,
            size: 4.0.into(),
            angle: None,
        }),
        stroke_gap: Some(mars_style::StrokeGap {
            interval_px: 12.0,
            initial_px: 3.0,
        }),
        ..Default::default()
    };
    let gap = r(&s).stroke.expect("stroke").gap.expect("gap");
    assert!((gap.interval_px - 12.0).abs() < 1e-6);
    assert!((gap.initial_px - 3.0).abs() < 1e-6);
}

#[test]
fn stroke_gap_drops_when_marker_absent() {
    let s = Style {
        stroke: Some(Colour::rgba(0, 0, 0, 255)),
        stroke_width: Some(1.0.into()),
        stroke_gap: Some(mars_style::StrokeGap {
            interval_px: 12.0,
            initial_px: 0.0,
        }),
        ..Default::default()
    };
    assert!(r(&s).stroke.expect("stroke").gap.is_none());
}

#[test]
fn stroke_gap_drops_when_interval_non_positive() {
    let s = Style {
        stroke: Some(Colour::rgba(0, 0, 0, 255)),
        stroke_width: Some(1.0.into()),
        marker: Some(mars_style::MarkerSymbol {
            shape: mars_style::MarkerShape::Circle,
            size: 4.0.into(),
            angle: None,
        }),
        stroke_gap: Some(mars_style::StrokeGap {
            interval_px: 0.0,
            initial_px: 0.0,
        }),
        ..Default::default()
    };
    assert!(r(&s).stroke.expect("stroke").gap.is_none());
}

#[test]
fn stroke_offset_zero_when_tiny() {
    let s = Style {
        stroke: Some(Colour::rgba(0, 0, 0, 255)),
        stroke_width: Some(1.0.into()),
        stroke_offset_px: Some(0.0),
        ..Default::default()
    };
    assert_eq!(r(&s).stroke.expect("stroke").offset_px, 0.0);
}
