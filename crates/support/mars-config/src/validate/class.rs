use std::collections::{BTreeMap, BTreeSet};

use crate::ConfigError;
use crate::model::{ClassStyle, Layer, StyleEntry};
use crate::validate::band::intersect_scale_windows;

pub(super) fn validate_classes(layer: &Layer, styles: &BTreeMap<String, StyleEntry>) -> Result<(), ConfigError> {
    validate_unique_class_names(layer)?;
    validate_class_count(layer)?;
    for class in &layer.classes {
        validate_class_style_ref(layer, class, styles)?;
        validate_class_when(layer, class)?;
        validate_class_scale(layer, class)?;
    }
    Ok(())
}

fn validate_unique_class_names(layer: &Layer) -> Result<(), ConfigError> {
    // duplicate class makes the second class unreachable under first-match-wins;
    // almost never intentional.
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for class in &layer.classes {
        if !seen.insert(class.name.as_str()) {
            return Err(ConfigError::Invalid(format!(
                "layer {} declares class {:?} more than once",
                layer.name, class.name
            )));
        }
    }
    Ok(())
}

fn validate_class_count(layer: &Layer) -> Result<(), ConfigError> {
    // sidecar assignments are u16-indexed and the optional label's
    // style_ref_idx is appended after the class style refs, so classes.len()
    // must itself fit in u16. without this check assign_class silently returns
    // None past u16::MAX and the label idx saturates, aliasing styles.
    if layer.classes.len() > u16::MAX as usize {
        return Err(ConfigError::Invalid(format!(
            "layer {} declares {} classes; the per-layer limit is {}",
            layer.name,
            layer.classes.len(),
            u16::MAX
        )));
    }
    Ok(())
}

fn validate_class_style_ref(
    layer: &Layer,
    class: &crate::model::Class,
    styles: &BTreeMap<String, StyleEntry>,
) -> Result<(), ConfigError> {
    if let ClassStyle::Ref { name } = &class.style
        && !styles.contains_key(name)
    {
        return Err(ConfigError::Invalid(format!(
            "layer {} class {:?} references unknown style {:?}",
            layer.name, class.name, name
        )));
    }
    Ok(())
}

fn validate_class_when(layer: &Layer, class: &crate::model::Class) -> Result<(), ConfigError> {
    if let Some(when) = &class.when
        && let Err(e) = mars_expr::parse(when)
    {
        return Err(ConfigError::Invalid(format!(
            "layer {} class {:?} when: parse error: {e}",
            layer.name, class.name
        )));
    }
    Ok(())
}

fn validate_class_scale(layer: &Layer, class: &crate::model::Class) -> Result<(), ConfigError> {
    let Some(cs) = &class.scale else {
        return Ok(());
    };
    if let (Some(a), Some(b)) = (cs.min, cs.max)
        && a >= b
    {
        return Err(ConfigError::Invalid(format!(
            "layer {} class {:?} scale window is empty: min {a} >= max {b}",
            layer.name, class.name
        )));
    }
    if let Some(ls) = &layer.scale
        && intersect_scale_windows(ls, cs).is_none()
    {
        return Err(ConfigError::Invalid(format!(
            "layer {} class {:?} scale window is disjoint from layer scale window",
            layer.name, class.name
        )));
    }
    Ok(())
}
