use std::collections::BTreeMap;

use crate::ConfigError;
use crate::model::{LabelStyleAttach, Layer, LayerLabel, StyleEntry};

pub(super) fn validate_label(layer: &Layer, styles: &BTreeMap<String, StyleEntry>) -> Result<(), ConfigError> {
    if let Some(label) = &layer.label {
        validate_one(layer, None, label, styles)?;
    }
    for class in &layer.classes {
        if let Some(label) = &class.label {
            validate_one(layer, Some(class.name.as_str()), label, styles)?;
        }
    }
    Ok(())
}

fn validate_one(
    layer: &Layer,
    class: Option<&str>,
    label: &LayerLabel,
    styles: &BTreeMap<String, StyleEntry>,
) -> Result<(), ConfigError> {
    let scope = match class {
        Some(c) => format!("layer {} class {:?} label", layer.name, c),
        None => format!("layer {} label", layer.name),
    };

    if let LabelStyleAttach::Ref { name } = &label.style
        && !matches!(styles.get(name), Some(StyleEntry::Label(_)))
    {
        return Err(ConfigError::Invalid(format!(
            "{scope} references unknown or non-label style {name:?}"
        )));
    }

    if let Some(placement) = &label.placement {
        let geom = mars_style::LayerGeomKind::parse(layer.kind.as_str());
        // unknown layer kind is rejected by other validation paths; here we
        // only flag explicit kind/placement mismatches.
        let ok = matches!(
            (geom, placement),
            (Some(mars_style::LayerGeomKind::Point), mars_style::Placement::Point)
                | (
                    Some(mars_style::LayerGeomKind::Line),
                    mars_style::Placement::Line { .. }
                )
                | (
                    Some(mars_style::LayerGeomKind::Polygon),
                    mars_style::Placement::Polygon { .. }
                )
                | (None, _)
        );
        if !ok {
            return Err(ConfigError::Invalid(format!(
                "{scope} placement does not match geometry type {:?}",
                layer.kind
            )));
        }
    }
    Ok(())
}
