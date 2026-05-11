use std::collections::BTreeMap;

use crate::ConfigError;
use crate::model::{LabelStyleAttach, Layer, StyleEntry};

pub(super) fn validate_label(layer: &Layer, styles: &BTreeMap<String, StyleEntry>) -> Result<(), ConfigError> {
    let Some(label) = &layer.label else {
        return Ok(());
    };

    if let LabelStyleAttach::Ref { name } = &label.style
        && !matches!(styles.get(name), Some(StyleEntry::Label(_)))
    {
        return Err(ConfigError::Invalid(format!(
            "layer {} label references unknown or non-label style {:?}",
            layer.name, name
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
                "layer {} placement does not match geometry type {:?}",
                layer.name, layer.kind
            )));
        }
    }
    Ok(())
}
