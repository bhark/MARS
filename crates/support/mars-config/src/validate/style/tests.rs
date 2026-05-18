#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use mars_style::{Colour, StrokeGap, Style};

fn polygon_style(fill: FillPaint) -> StyleEntry {
    StyleEntry::Polygon(Style {
        fill: Some(fill),
        ..Default::default()
    })
}

fn point_style(marker: MarkerSymbol) -> StyleEntry {
    StyleEntry::Point(Style {
        marker: Some(marker),
        ..Default::default()
    })
}

fn line_style_with_gap(marker: Option<MarkerSymbol>, gap: StrokeGap) -> StyleEntry {
    StyleEntry::Line(Style {
        stroke: Some(Colour::rgb(0, 0, 0)),
        stroke_width: Some(1.0.into()),
        marker,
        stroke_gap: Some(gap),
        ..Default::default()
    })
}

#[test]
fn accepts_well_formed_hatch_and_marker() {
    let mut styles = BTreeMap::new();
    styles.insert(
        "h".into(),
        polygon_style(FillPaint::Hatch {
            spacing: 4.0,
            angle_deg: 45.0,
            line_width: 0.5,
            colour: Colour::rgb(0, 0, 0),
        }),
    );
    styles.insert(
        "m".into(),
        point_style(MarkerSymbol {
            shape: mars_style::MarkerShape::Circle,
            size: 6.0.into(),
            angle: None,
        }),
    );
    validate_styles(&styles).unwrap();
}

#[test]
fn rejects_zero_hatch_spacing() {
    let mut styles = BTreeMap::new();
    styles.insert(
        "bad".into(),
        polygon_style(FillPaint::Hatch {
            spacing: 0.0,
            angle_deg: 45.0,
            line_width: 0.5,
            colour: Colour::rgb(0, 0, 0),
        }),
    );
    let err = validate_styles(&styles).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("hatch.spacing"), "got: {msg}");
}

#[test]
fn rejects_negative_hatch_line_width() {
    let mut styles = BTreeMap::new();
    styles.insert(
        "bad".into(),
        polygon_style(FillPaint::Hatch {
            spacing: 4.0,
            angle_deg: 45.0,
            line_width: -1.0,
            colour: Colour::rgb(0, 0, 0),
        }),
    );
    let err = validate_styles(&styles).unwrap_err();
    assert!(err.to_string().contains("hatch.line_width"), "{err}");
}

#[test]
fn rejects_non_finite_hatch_angle() {
    let mut styles = BTreeMap::new();
    styles.insert(
        "bad".into(),
        polygon_style(FillPaint::Hatch {
            spacing: 4.0,
            angle_deg: f32::NAN,
            line_width: 0.5,
            colour: Colour::rgb(0, 0, 0),
        }),
    );
    let err = validate_styles(&styles).unwrap_err();
    assert!(err.to_string().contains("hatch.angle_deg"), "{err}");
}

#[test]
fn rejects_zero_marker_size() {
    let mut styles = BTreeMap::new();
    styles.insert(
        "bad".into(),
        point_style(MarkerSymbol {
            shape: mars_style::MarkerShape::Pin,
            size: 0.0.into(),
            angle: None,
        }),
    );
    let err = validate_styles(&styles).unwrap_err();
    assert!(err.to_string().contains("marker.size"), "{err}");
}

#[test]
fn rejects_glyph_marker_with_empty_ch() {
    let mut styles = BTreeMap::new();
    styles.insert(
        "bad".into(),
        point_style(MarkerSymbol {
            shape: mars_style::MarkerShape::Glyph {
                font_family: "Sans".into(),
                ch: String::new(),
            },
            size: 12.0.into(),
            angle: None,
        }),
    );
    let err = validate_styles(&styles).unwrap_err();
    assert!(err.to_string().contains("marker.ch"), "{err}");
}

#[test]
fn rejects_glyph_marker_with_non_solid_fill() {
    let mut styles = BTreeMap::new();
    styles.insert(
        "hatch".into(),
        StyleEntry::Point(Style {
            fill: Some(FillPaint::Hatch {
                spacing: 4.0,
                angle_deg: 45.0,
                line_width: 0.5,
                colour: Colour::rgb(0, 0, 0),
            }),
            marker: Some(MarkerSymbol {
                shape: mars_style::MarkerShape::Glyph {
                    font_family: "Sans".into(),
                    ch: "A".into(),
                },
                size: 12.0.into(),
                angle: None,
            }),
            ..Default::default()
        }),
    );
    let err = validate_styles(&styles).unwrap_err();
    assert!(err.to_string().contains("non-solid"), "{err}");

    let mut styles = BTreeMap::new();
    styles.insert(
        "img".into(),
        StyleEntry::Point(Style {
            fill: Some(FillPaint::Image { name: "pattern".into() }),
            marker: Some(MarkerSymbol {
                shape: mars_style::MarkerShape::Glyph {
                    font_family: "Sans".into(),
                    ch: "A".into(),
                },
                size: 12.0.into(),
                angle: None,
            }),
            ..Default::default()
        }),
    );
    let err = validate_styles(&styles).unwrap_err();
    assert!(err.to_string().contains("non-solid"), "{err}");
}

#[test]
fn accepts_well_formed_stroke_gap() {
    let mut styles = BTreeMap::new();
    styles.insert(
        "ok".into(),
        line_style_with_gap(
            Some(MarkerSymbol {
                shape: mars_style::MarkerShape::Circle,
                size: 4.0.into(),
                angle: None,
            }),
            StrokeGap {
                interval_px: 12.0,
                initial_px: 3.0,
            },
        ),
    );
    validate_styles(&styles).unwrap();
}

#[test]
fn rejects_zero_stroke_gap_interval() {
    let mut styles = BTreeMap::new();
    styles.insert(
        "bad".into(),
        line_style_with_gap(
            Some(MarkerSymbol {
                shape: mars_style::MarkerShape::Circle,
                size: 4.0.into(),
                angle: None,
            }),
            StrokeGap {
                interval_px: 0.0,
                initial_px: 0.0,
            },
        ),
    );
    let err = validate_styles(&styles).unwrap_err();
    assert!(err.to_string().contains("stroke_gap.interval_px"), "{err}");
}

#[test]
fn rejects_negative_initial_gap() {
    let mut styles = BTreeMap::new();
    styles.insert(
        "bad".into(),
        line_style_with_gap(
            Some(MarkerSymbol {
                shape: mars_style::MarkerShape::Circle,
                size: 4.0.into(),
                angle: None,
            }),
            StrokeGap {
                interval_px: 10.0,
                initial_px: -1.0,
            },
        ),
    );
    let err = validate_styles(&styles).unwrap_err();
    assert!(err.to_string().contains("stroke_gap.initial_px"), "{err}");
}

#[test]
fn rejects_stroke_gap_without_marker() {
    let mut styles = BTreeMap::new();
    styles.insert(
        "bad".into(),
        line_style_with_gap(
            None,
            StrokeGap {
                interval_px: 10.0,
                initial_px: 0.0,
            },
        ),
    );
    let err = validate_styles(&styles).unwrap_err();
    assert!(err.to_string().contains("stroke_gap"), "{err}");
    assert!(err.to_string().contains("marker"), "{err}");
}
