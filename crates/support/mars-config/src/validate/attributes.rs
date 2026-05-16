use std::collections::BTreeSet;

use crate::ConfigError;
use crate::model::{Layer, SourceBinding};

/// Every attribute referenced by a class `when:` or label `text:` must be
/// declared on every binding for the layer; otherwise the snapshot path would
/// silently observe a missing column at eval time. Binding `filter:`
/// expressions are checked against the same per-binding allowlist used by the
/// SQL lowering pass, so a bad filter fails at config time, not at SQL build.
pub(super) fn validate_attribute_references(layer: &Layer) -> Result<(), ConfigError> {
    if let Some(template) = &layer.template {
        mars_expr::parse_template(template).map_err(|e| {
            ConfigError::Invalid(format!("layer {} template parse error: {e}", layer.name))
        })?;
    }

    let referenced = collect_referenced_attributes(layer);

    for (i, binding) in layer.sources.iter().enumerate() {
        let declared: BTreeSet<&str> = binding.attributes.iter().map(String::as_str).collect();
        check_declared_covers(layer, i, binding, &declared, &referenced)?;
        check_filter_idents(layer, i, binding, &declared)?;
    }
    Ok(())
}

fn collect_referenced_attributes(layer: &Layer) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for class in &layer.classes {
        if let Some(when) = &class.when
            && let Ok(expr) = mars_expr::parse(when)
        {
            mars_expr::collect_idents(&expr, &mut out);
        }
        if let Some(label) = &class.label {
            collect_template_idents(&label.text, &mut out);
        }
    }
    if let Some(label) = &layer.label {
        collect_template_idents(&label.text, &mut out);
    }
    if let Some(template) = &layer.template {
        collect_template_idents(template, &mut out);
    }
    out
}

fn collect_template_idents(text: &str, out: &mut BTreeSet<String>) {
    if let Ok(template) = mars_expr::parse_template(text) {
        for seg in &template.segments {
            if let mars_expr::Segment::Ident(name) = seg {
                out.insert(name.clone());
            }
        }
    }
}

fn check_declared_covers(
    layer: &Layer,
    i: usize,
    binding: &SourceBinding,
    declared: &BTreeSet<&str>,
    referenced: &BTreeSet<String>,
) -> Result<(), ConfigError> {
    for name in referenced {
        if !declared.contains(name.as_str()) {
            return Err(ConfigError::Invalid(format!(
                "layer {} source[{i}] (from {:?}) does not declare attribute {name:?} \
                 referenced by a class when: or label text",
                layer.name,
                binding.source_descriptor()
            )));
        }
    }
    Ok(())
}

fn check_filter_idents(
    layer: &Layer,
    i: usize,
    binding: &SourceBinding,
    declared: &BTreeSet<&str>,
) -> Result<(), ConfigError> {
    let Some(filter) = &binding.filter else {
        return Ok(());
    };
    let expr = mars_expr::parse(filter).map_err(|e| {
        ConfigError::Invalid(format!(
            "layer {} source[{i}] (from {:?}) filter parse error: {e}",
            layer.name,
            binding.source_descriptor()
        ))
    })?;
    let mut idents: BTreeSet<String> = BTreeSet::new();
    mars_expr::collect_idents(&expr, &mut idents);
    for name in &idents {
        let in_attrs = declared.contains(name.as_str());
        let is_id = binding.id_column.as_deref() == Some(name.as_str());
        if !in_attrs && !is_id {
            return Err(ConfigError::Invalid(format!(
                "layer {} source[{i}] (from {:?}) filter references unknown ident {name:?}; \
                 declare it in `attributes` or as `id_column`",
                layer.name,
                binding.source_descriptor()
            )));
        }
    }
    Ok(())
}
