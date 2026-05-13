//! CLASS block parser. Walks a class body, accumulates STYLE blocks, and
//! emits a [`ClassSkeleton`] plus a deduplicated [`StyleDef`] entry on the
//! [`Skeleton`].

use tracing::warn;

use crate::directive::ClassDirective;
use crate::emitter::{ClassSkeleton, Skeleton, StyleDef, slugify};
use crate::scanner::{Token, block_range, is_block_opener};

use super::is_unsupported;
use super::style_block::{StyleBlock, canonical_signature, collapse_styles, parse_style_block};

pub(crate) fn parse_class(
    body: &[Token],
    class_line: usize,
    layer_name: &str,
    geom_kind: &str,
    skel: &mut Skeleton,
) -> Option<ClassSkeleton> {
    let mut name: Option<String> = None;
    let mut expression: Option<String> = None;
    let mut styles: Vec<StyleBlock> = Vec::new();
    let mut min_scale_denom: Option<u64> = None;
    let mut max_scale_denom: Option<u64> = None;

    let mut i = 0;
    while i < body.len() {
        let t = &body[i];
        match ClassDirective::from_token(t, is_unsupported) {
            ClassDirective::Name(t) if name.is_none() => name = t.args.first().cloned(),
            ClassDirective::MinScaleDenom(t) => {
                if let Some(n) = parse_class_scale_denom(t) {
                    min_scale_denom = Some(n);
                }
            }
            ClassDirective::MaxScaleDenom(t) => {
                if let Some(n) = parse_class_scale_denom(t) {
                    max_scale_denom = Some(n);
                }
            }
            ClassDirective::Expression(t) => {
                let joined = t.args.join(" ");
                match crate::expression::parse_mapfile_expression(&joined, t.line) {
                    Ok(expr) => {
                        expression = Some(format!("{expr}"));
                    }
                    Err(e) => {
                        warn!(line = t.line, error = %e, "could not parse EXPRESSION");
                        expression = Some(format!("# TODO: hand-translate: {joined}"));
                    }
                }
            }
            ClassDirective::Style => {
                if let Some(r) = block_range(body, i) {
                    styles.push(parse_style_block(&body[r.start + 1..r.end - 1]));
                    i = r.end;
                    continue;
                }
            }
            ClassDirective::Unsupported(t) => {
                warn!(line = t.line, keyword = %t.keyword, "unsupported class-level construct");
                if is_block_opener(&t.keyword)
                    && let Some(r) = block_range(body, i)
                {
                    i = r.end;
                    continue;
                }
            }
            // re-occurrence of NAME after the first is ignored; same for any
            // keyword we don't understand inside a CLASS block.
            ClassDirective::Name(_) | ClassDirective::Unknown => {}
        }
        i += 1;
    }

    let title = name.clone();
    let class_name = slugify(&name.unwrap_or_else(|| format!("class_l{class_line}")));
    let style_prefix = if geom_kind == "polygon" { "poly" } else { geom_kind };
    let style_name = format!("{}_{}_{}", style_prefix, slugify(layer_name), class_name);

    let collapsed = collapse_styles(&styles, class_line, &skel.symbols);

    let canonical = canonical_signature(
        geom_kind,
        collapsed.fill.as_ref(),
        collapsed.stroke.as_ref(),
        collapsed.width,
        collapsed.dasharray.as_ref(),
        collapsed.marker.as_ref(),
        collapsed.opacity,
        collapsed.stroke_offset_px,
        collapsed.stroke_gap.as_ref(),
        collapsed.stroke_linejoin,
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
        ) == canonical
    });

    let style_ref = if let Some(st) = existing {
        st.name.clone()
    } else {
        skel.styles.push(StyleDef {
            name: style_name.clone(),
            style_type: geom_kind.to_string(),
            fill: collapsed.fill,
            stroke: collapsed.stroke,
            stroke_width: collapsed.width,
            stroke_dasharray: collapsed.dasharray,
            stroke_linejoin: collapsed.stroke_linejoin,
            marker: collapsed.marker,
            opacity: collapsed.opacity,
            stroke_offset_px: collapsed.stroke_offset_px,
            stroke_gap: collapsed.stroke_gap,
            font_family: None,
            font_size: None,
            halo_color: None,
            halo_width: None,
            priority: None,
            min_distance: None,
        });
        style_name
    };

    Some(ClassSkeleton {
        name: class_name,
        title,
        when: expression,
        min_scale_denom,
        max_scale_denom,
        style_ref,
    })
}

fn parse_class_scale_denom(t: &Token) -> Option<u64> {
    let arg = t.args.first()?;
    match arg.parse::<f64>() {
        Ok(v) if v.is_finite() && v >= 0.0 => Some(super::normalize_n_plus_one(v as u64)),
        _ => {
            warn!(line = t.line, keyword = %t.keyword, value = %arg, "could not parse class scale denom");
            None
        }
    }
}
