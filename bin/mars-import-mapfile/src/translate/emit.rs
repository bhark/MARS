//! `Resolved*` -> [`Skeleton`] emission. Reads non-Option fields produced by
//! [`super::resolved`]; never calls `.unwrap_or(default)` itself.
//!
//! Layered shape mirrors `mars-render`: defaults belong in `resolved.rs`,
//! mapping to wire types belongs here. The single per-layer "unimplemented
//! directives" warn is fired from [`emit_layer`], matching the
//! `refactor(render): warn once per render` pattern.

use tracing::warn;

use crate::emitter::{
    ClassSkeleton, ClassStyleAttach, EmitFill, LabelSkeleton, LayerSkeleton, Skeleton, SourceSkeleton, StyleDef,
};

use super::resolved::{ResolvedClass, ResolvedLabel, ResolvedLayer, ResolvedSymbol};
use super::style_block::{SinglePass, canonical_signature};

pub(crate) fn emit_layer(r: ResolvedLayer, skel: &mut Skeleton) {
    if !r.unimplemented.is_empty() {
        warn!(
            layer = %r.name,
            dropped = ?r.unimplemented,
            "layer has unimplemented STYLE/LABEL directives; dropping"
        );
    }

    let classes: Vec<ClassSkeleton> = r.classes.into_iter().map(|rc| emit_class(rc, skel)).collect();
    let label = r.label.map(|rl| emit_label(rl, skel));

    let attributes = r.attributes;
    let sources = r
        .sources
        .into_iter()
        .map(|rs| SourceSkeleton {
            max_denom_exclusive: rs.max_denom_exclusive,
            source: rs.source,
            filter: rs.filter,
            geometry_column: rs.geometry_column,
            id_column: rs.id_column,
            attributes: attributes.clone(),
        })
        .collect();

    skel.layers.push(LayerSkeleton {
        name: r.name,
        title: r.title,
        abstract_: r.abstract_,
        geom_kind: r.geom_kind,
        sources,
        classes,
        label,
        group: r.group_path,
        wms: r.wms,
    });
}

fn emit_class(r: ResolvedClass, skel: &mut Skeleton) -> ClassSkeleton {
    // single-pass: emit one StyleDef into the registry, dedup by canonical
    // signature; the class refers via ClassStyleAttach::Ref. multi-pass: each
    // SinglePass becomes one inline StyleDef; the class carries them through
    // ClassStyleAttach::Passes and no registry entry is created. zero-pass
    // (a CLASS with no STYLE block) falls back to a single empty Ref entry so
    // the class always has a renderable style attachment.
    let style = match r.passes.len() {
        0 | 1 => ClassStyleAttach::Ref(emit_single_pass_to_registry(
            &r.style_type,
            &r.style_name,
            r.passes.into_iter().next().unwrap_or_default(),
            skel,
        )),
        _ => ClassStyleAttach::Passes(
            r.passes
                .into_iter()
                .map(|p| single_pass_to_style_def(&r.style_type, String::new(), p))
                .collect(),
        ),
    };

    let label = r.label.map(|rl| emit_label(rl, skel));

    ClassSkeleton {
        name: r.class_name,
        title: r.title,
        when: r.when,
        min_scale_denom: r.min_scale_denom,
        max_scale_denom: r.max_scale_denom,
        style,
        label,
    }
}

/// push or dedup a single-pass [`StyleDef`] into [`Skeleton::styles`] and
/// return the name the class should reference. dedup uses
/// [`canonical_signature`] over the wire-facing fields.
fn emit_single_pass_to_registry(style_type: &str, style_name: &str, pass: SinglePass, skel: &mut Skeleton) -> String {
    let canonical = canonical_signature(
        style_type,
        pass.fill.as_ref(),
        pass.stroke.as_ref(),
        pass.width,
        pass.dasharray.as_ref(),
        pass.marker.as_ref(),
        pass.opacity,
        pass.stroke_offset_px,
        pass.stroke_gap.as_ref(),
        pass.stroke_linejoin,
        pass.geom_transform,
        pass.min_feature_size_px,
    );
    if let Some(st) = skel.styles.iter().find(|s| {
        canonical_signature(
            &s.style_type,
            s.fill.as_ref(),
            s.stroke.as_ref(),
            s.stroke_width,
            s.stroke_dasharray.as_ref(),
            s.marker.as_ref(),
            s.opacity,
            s.stroke_offset_px,
            s.stroke_gap.as_ref(),
            s.stroke_linejoin,
            s.geom_transform,
            s.min_feature_size_px,
        ) == canonical
    }) {
        return st.name.clone();
    }
    skel.styles
        .push(single_pass_to_style_def(style_type, style_name.to_string(), pass));
    style_name.to_string()
}

/// Lower a [`SinglePass`] to a [`StyleDef`]. `name` is used only by the
/// registry path; inline-pass entries pass an empty string.
fn single_pass_to_style_def(style_type: &str, name: String, pass: SinglePass) -> StyleDef {
    StyleDef {
        name,
        style_type: style_type.to_string(),
        fill: pass.fill,
        stroke: pass.stroke,
        stroke_width: pass.width,
        stroke_dasharray: pass.dasharray,
        stroke_linejoin: pass.stroke_linejoin,
        marker: pass.marker,
        opacity: pass.opacity,
        stroke_offset_px: pass.stroke_offset_px,
        stroke_gap: pass.stroke_gap,
        geom_transform: pass.geom_transform,
        min_feature_size_px: pass.min_feature_size_px,
        font_family: None,
        font_size: None,
        halo_color: None,
        halo_width: None,
        priority: None,
        min_distance: None,
        position: None,
        offset_px: None,
        angle_deg: None,
        angle_attribute: None,
        partials: None,
        force: None,
    }
}

fn emit_label(r: ResolvedLabel, skel: &mut Skeleton) -> LabelSkeleton {
    skel.styles.push(StyleDef {
        name: r.style_name.clone(),
        style_type: "label".into(),
        fill: Some(EmitFill::Hex(r.fill)),
        stroke: None,
        stroke_width: None,
        stroke_dasharray: None,
        stroke_linejoin: None,
        marker: None,
        opacity: None,
        stroke_offset_px: None,
        stroke_gap: None,
        geom_transform: None,
        min_feature_size_px: None,
        font_family: Some(r.font_family),
        font_size: Some(r.font_size),
        halo_color: r.halo_color,
        halo_width: r.halo_width,
        priority: r.priority,
        min_distance: r.min_distance,
        position: r.position,
        offset_px: r.offset_px,
        angle_deg: r.angle_deg,
        angle_attribute: r.angle_attribute,
        partials: r.partials,
        force: r.force,
    });

    LabelSkeleton {
        text: r.text,
        style_ref: r.style_name,
        placement_line: r.placement_line,
    }
}

pub(crate) fn emit_symbol(r: ResolvedSymbol, skel: &mut Skeleton) {
    skel.symbols.insert(r.name, r.def);
}
