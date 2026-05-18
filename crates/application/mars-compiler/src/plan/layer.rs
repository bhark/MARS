//! per-layer plan helpers. parses `when:` expressions and `text:` templates
//! once at plan-build time, synthesises inline style refs, and resolves
//! label-style attachments against the config's `styles:` table.

use mars_config::{Config, LabelStyleAttach, Layer as CfgLayer};
use mars_expr::{parse, parse_template};
use mars_style::{LabelStyle, default_placement};
use mars_types::{BindingId, LayerId};

use super::error::PlanError;
use super::types::{ClassPlan, LayerLabelPlan, LayerPlan};

pub(super) fn build_layer_plan(cfg: &Config, layer: &CfgLayer, binding_id: &BindingId) -> Result<LayerPlan, PlanError> {
    let mut classes: Vec<ClassPlan> = Vec::with_capacity(layer.classes.len());
    for class in &layer.classes {
        let when = match &class.when {
            Some(s) => Some(parse(s).map_err(|source| PlanError::ClassWhenParse {
                layer: layer.name.clone(),
                class: class.name.clone(),
                source,
            })?),
            None => None,
        };
        let style_ref = match &class.style {
            mars_config::ClassStyle::Ref { name } => name.clone(),
            // both single-inline and multi-pass classes synthesise the same
            // per-class style name; the bin-side stylesheet builder writes
            // the passes (or single style as a one-element slice) under it.
            mars_config::ClassStyle::Inline(_) | mars_config::ClassStyle::Passes { .. } => {
                format!("{layer}__{class}", layer = layer.name, class = class.name)
            }
        };
        let label = class
            .label
            .as_ref()
            .map(|l| build_class_label_plan(cfg, layer, &class.name, l))
            .transpose()?;
        classes.push(ClassPlan {
            name: class.name.clone(),
            when,
            style_ref,
            label,
        });
    }

    let label = layer
        .label
        .as_ref()
        .map(|l| build_label_plan(cfg, layer, l))
        .transpose()?;

    Ok(LayerPlan {
        layer_id: layer.name.clone(),
        binding_id: binding_id.clone(),
        kind: layer.kind.clone(),
        classes,
        label,
        label_survival: layer.label_survival,
    })
}

fn build_label_plan(
    cfg: &Config,
    layer: &CfgLayer,
    label: &mars_config::LayerLabel,
) -> Result<LayerLabelPlan, PlanError> {
    let template = parse_template(&label.text).map_err(|source| PlanError::LabelTemplateParse {
        layer: layer.name.clone(),
        source,
    })?;
    let inline_style_ref = format!("{layer}__label", layer = layer.name);
    let (style_ref, style) = resolve_label_style(cfg, &layer.name, &inline_style_ref, &label.style)?;
    let placement = label.placement.clone().unwrap_or_else(|| {
        let kind = mars_style::LayerGeomKind::parse(layer.kind.as_str()).unwrap_or(mars_style::LayerGeomKind::Point);
        default_placement(kind)
    });
    Ok(LayerLabelPlan {
        style_ref,
        style,
        text: template,
        placement,
    })
}

fn build_class_label_plan(
    cfg: &Config,
    layer: &CfgLayer,
    class_name: &str,
    label: &mars_config::LayerLabel,
) -> Result<LayerLabelPlan, PlanError> {
    let template = parse_template(&label.text).map_err(|source| PlanError::LabelTemplateParse {
        layer: layer.name.clone(),
        source,
    })?;
    let inline_style_ref = format!("{layer}__{class_name}__label", layer = layer.name);
    let (style_ref, style) = resolve_label_style(cfg, &layer.name, &inline_style_ref, &label.style)?;
    let placement = label.placement.clone().unwrap_or_else(|| {
        let kind = mars_style::LayerGeomKind::parse(layer.kind.as_str()).unwrap_or(mars_style::LayerGeomKind::Point);
        default_placement(kind)
    });
    Ok(LayerLabelPlan {
        style_ref,
        style,
        text: template,
        placement,
    })
}

fn resolve_label_style(
    cfg: &Config,
    layer_name: &LayerId,
    inline_style_ref: &str,
    attach: &LabelStyleAttach,
) -> Result<(String, LabelStyle), PlanError> {
    match attach {
        LabelStyleAttach::Ref { name } => {
            let style = cfg
                .styles
                .get(name)
                .and_then(|e| e.as_label().cloned())
                .ok_or_else(|| PlanError::UnknownLabelStyleRef {
                    layer: layer_name.clone(),
                    name: name.clone(),
                })?;
            Ok((name.clone(), style))
        }
        LabelStyleAttach::Inline(style) => Ok((inline_style_ref.to_string(), style.clone())),
    }
}
