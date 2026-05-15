//! `Resolved*` -> [`Skeleton`] emission. Reads non-Option fields produced by
//! [`super::resolved`]; never calls `.unwrap_or(default)` itself.
//!
//! Layered shape mirrors `mars-render`: defaults belong in `resolved.rs`,
//! mapping to wire types belongs here. The single per-layer "unimplemented
//! directives" warn is fired from [`emit_layer`], matching the
//! `refactor(render): warn once per render` pattern.

use tracing::warn;

use crate::emitter::{ClassSkeleton, EmitFill, LabelSkeleton, LayerSkeleton, Skeleton, SourceSkeleton, StyleDef};

use super::resolved::{ResolvedClass, ResolvedLabel, ResolvedLayer, ResolvedSymbol};
use super::style_block::canonical_signature;

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
        geom_kind: r.geom_kind,
        sources,
        classes,
        label,
    });
}

fn emit_class(r: ResolvedClass, skel: &mut Skeleton) -> ClassSkeleton {
    let canonical = canonical_signature(
        &r.style_type,
        r.collapsed.fill.as_ref(),
        r.collapsed.stroke.as_ref(),
        r.collapsed.width,
        r.collapsed.dasharray.as_ref(),
        r.collapsed.marker.as_ref(),
        r.collapsed.opacity,
        r.collapsed.stroke_offset_px,
        r.collapsed.stroke_gap.as_ref(),
        r.collapsed.stroke_linejoin,
        r.collapsed.geom_transform,
    );

    let existing = skel.styles.iter().find(|s| {
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
        ) == canonical
    });

    let style_ref = if let Some(st) = existing {
        st.name.clone()
    } else {
        skel.styles.push(StyleDef {
            name: r.style_name.clone(),
            style_type: r.style_type,
            fill: r.collapsed.fill,
            stroke: r.collapsed.stroke,
            stroke_width: r.collapsed.width,
            stroke_dasharray: r.collapsed.dasharray,
            stroke_linejoin: r.collapsed.stroke_linejoin,
            marker: r.collapsed.marker,
            opacity: r.collapsed.opacity,
            stroke_offset_px: r.collapsed.stroke_offset_px,
            stroke_gap: r.collapsed.stroke_gap,
            geom_transform: r.collapsed.geom_transform,
            font_family: None,
            font_size: None,
            halo_color: None,
            halo_width: None,
            priority: None,
            min_distance: None,
        });
        r.style_name
    };

    ClassSkeleton {
        name: r.class_name,
        title: r.title,
        when: r.when,
        min_scale_denom: r.min_scale_denom,
        max_scale_denom: r.max_scale_denom,
        style_ref,
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
        font_family: Some(r.font_family),
        font_size: Some(r.font_size),
        halo_color: r.halo_color,
        halo_width: r.halo_width,
        priority: r.priority,
        min_distance: r.min_distance,
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
