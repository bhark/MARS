//! Cross-cutting style validators that exceed serde's reach: numeric bounds
//! on hatch + marker fields. Caught at config-load so the renderer never
//! sees zero / negative / non-finite values.

use std::collections::BTreeMap;

use mars_style::{FillPaint, MarkerSymbol};

use crate::ConfigError;
use crate::model::StyleEntry;

pub(super) fn validate_styles(styles: &BTreeMap<String, StyleEntry>) -> Result<(), ConfigError> {
    for (name, entry) in styles {
        if let Some(s) = entry.as_geometry() {
            if let Some(fp) = &s.fill {
                validate_fill_paint(name, fp)?;
            }
            if let Some(m) = &s.marker {
                validate_marker_symbol(name, m)?;
            }
        }
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

fn validate_marker_symbol(style_name: &str, m: &MarkerSymbol) -> Result<(), ConfigError> {
    let size = m.size();
    if !(size.is_finite() && size > 0.0) {
        return Err(ConfigError::Invalid(format!(
            "style {style_name:?} marker.size must be a finite positive number, got {size}"
        )));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use mars_style::{Colour, Style};

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
        styles.insert("m".into(), point_style(MarkerSymbol::Circle { size: 6.0 }));
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
        styles.insert("bad".into(), point_style(MarkerSymbol::Pin { size: 0.0 }));
        let err = validate_styles(&styles).unwrap_err();
        assert!(err.to_string().contains("marker.size"), "{err}");
    }
}
