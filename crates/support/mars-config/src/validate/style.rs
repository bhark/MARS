//! Cross-cutting style validators that exceed serde's reach: numeric bounds
//! on hatch + marker fields. Caught at config-load so the renderer never
//! sees zero / negative / non-finite values.

use std::collections::BTreeMap;

use mars_style::{FillPaint, MarkerSymbol, ScaledSize, StrokeGap};

use crate::ConfigError;
use crate::model::StyleEntry;

pub(super) fn validate_styles(styles: &BTreeMap<String, StyleEntry>) -> Result<(), ConfigError> {
    for (name, entry) in styles {
        if let StyleEntry::Passes { passes } = entry
            && passes.is_empty()
        {
            return Err(ConfigError::Invalid(format!(
                "style {name:?} declares an empty passes list; at least one pass is required"
            )));
        }
        if let Some(passes) = entry.as_geometry_passes() {
            for s in passes {
                if let Some(fp) = &s.fill {
                    validate_fill_paint(name, fp)?;
                }
                if let Some(m) = &s.marker {
                    validate_marker_symbol(name, m, s.fill.as_ref())?;
                }
                if let Some(w) = &s.stroke_width {
                    validate_scaled_size(name, "stroke_width", w)?;
                }
                if let Some(g) = &s.stroke_gap {
                    validate_stroke_gap(name, g, s.marker.is_some())?;
                }
                if let Some(t) = s.min_feature_size_px
                    && !(t.is_finite() && t > 0.0)
                {
                    return Err(ConfigError::Invalid(format!(
                        "style {name:?} min_feature_size_px must be a finite positive number, got {t}"
                    )));
                }
            }
        }
        if let Some(label) = entry.as_label() {
            validate_scaled_size(name, "font_size", &label.font_size)?;
        }
    }
    Ok(())
}

// shared `ScaledSize` numeric-bound checks: positive finite base, finite-and-
// ordered optional min/max. mirrors the renderer's behaviour: a degenerate
// authored value never reaches the resolve path.
fn validate_scaled_size(style_name: &str, field: &str, s: &ScaledSize) -> Result<(), ConfigError> {
    if !(s.base_px.is_finite() && s.base_px > 0.0) {
        return Err(ConfigError::Invalid(format!(
            "style {style_name:?} {field}.base_px must be a finite positive number, got {}",
            s.base_px
        )));
    }
    if let Some(v) = s.min_px
        && !(v.is_finite() && v > 0.0)
    {
        return Err(ConfigError::Invalid(format!(
            "style {style_name:?} {field}.min_px must be a finite positive number, got {v}"
        )));
    }
    if let Some(v) = s.max_px
        && !(v.is_finite() && v > 0.0)
    {
        return Err(ConfigError::Invalid(format!(
            "style {style_name:?} {field}.max_px must be a finite positive number, got {v}"
        )));
    }
    if let (Some(lo), Some(hi)) = (s.min_px, s.max_px)
        && lo > hi
    {
        return Err(ConfigError::Invalid(format!(
            "style {style_name:?} {field}.min_px ({lo}) must not exceed max_px ({hi})"
        )));
    }
    Ok(())
}

fn validate_fill_paint(style_name: &str, fp: &FillPaint) -> Result<(), ConfigError> {
    match fp {
        FillPaint::Solid(_) => Ok(()),
        FillPaint::Hatch {
            spacing,
            angle_deg,
            line_width,
            colour: _,
        } => {
            if !(spacing.is_finite() && *spacing > 0.0) {
                return Err(ConfigError::Invalid(format!(
                    "style {style_name:?} hatch.spacing must be a finite positive number, got {spacing}"
                )));
            }
            if !(line_width.is_finite() && *line_width > 0.0) {
                return Err(ConfigError::Invalid(format!(
                    "style {style_name:?} hatch.line_width must be a finite positive number, got {line_width}"
                )));
            }
            if !angle_deg.is_finite() {
                return Err(ConfigError::Invalid(format!(
                    "style {style_name:?} hatch.angle_deg must be finite, got {angle_deg}"
                )));
            }
            Ok(())
        }
        // image `name` is an unconstrained string today; the renderer-side
        // registry is the lookup gate. no numeric bounds to enforce here.
        FillPaint::Image { .. } => Ok(()),
    }
}

fn validate_marker_symbol(style_name: &str, m: &MarkerSymbol, fill: Option<&FillPaint>) -> Result<(), ConfigError> {
    validate_scaled_size(style_name, "marker.size", &m.size)?;
    // glyph markers shape a single character via the text path; an empty
    // string has no shape, and non-solid fills (hatch/image) have no
    // meaning for a single-glyph stamp.
    if let mars_style::MarkerShape::Glyph { ch, .. } = &m.shape {
        if ch.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "style {style_name:?} marker.ch must not be empty"
            )));
        }
        if let Some(FillPaint::Hatch { .. } | FillPaint::Image { .. }) = fill {
            return Err(ConfigError::Invalid(format!(
                "style {style_name:?} marker is Glyph but fill paint is non-solid; \
                 Glyph markers require a solid fill (or no fill)"
            )));
        }
    }
    Ok(())
}

fn validate_stroke_gap(style_name: &str, sg: &StrokeGap, has_marker: bool) -> Result<(), ConfigError> {
    if !(sg.interval_px.is_finite() && sg.interval_px > 0.0) {
        return Err(ConfigError::Invalid(format!(
            "style {style_name:?} stroke_gap.interval_px must be a finite positive number, got {}",
            sg.interval_px
        )));
    }
    if !(sg.initial_px.is_finite() && sg.initial_px >= 0.0) {
        return Err(ConfigError::Invalid(format!(
            "style {style_name:?} stroke_gap.initial_px must be a finite non-negative number, got {}",
            sg.initial_px
        )));
    }
    if !has_marker {
        return Err(ConfigError::Invalid(format!(
            "style {style_name:?} sets stroke_gap but no marker; stroke_gap stamps the marker along the line, so a marker is required"
        )));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
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
}
