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
mod tests;
