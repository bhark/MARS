//! ResolvedClass + the EXPRESSION-to-`when:` lowering. LayerComposite is
//! the layer-wide opacity/blend-mode that the layer threads in so the
//! class can fold it into each pass.

use std::collections::HashMap;

use tracing::warn;

use crate::emitter::{SymbolDef, slugify};

use super::super::class::{ParsedClass, ParsedExpression};
use super::super::style_block::{SinglePass, style_block_to_pass};
use super::label::{ResolvedLabel, class_label_style_name, resolve_label};

/// Layer-wide composite fields lifted from `COMPOSITE { ... }` and applied
/// to every pass at class-resolve time.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct LayerComposite {
    pub opacity: Option<f32>,
    pub blend_mode: Option<mars_style::BlendMode>,
}

#[derive(Debug)]
pub(crate) struct ResolvedClass {
    pub class_name: String,
    pub title: Option<String>,
    pub when: Option<String>,
    pub min_scale_denom: Option<u64>,
    pub max_scale_denom: Option<u64>,
    pub style_type: String,
    pub style_name: String,
    /// One [`SinglePass`] per parsed STYLE block, in declared order. Length
    /// 1 emits a single named [`StyleDef`] entry; length 2+ emits a
    /// `ClassStyleAttach::Passes` inline on the class.
    pub passes: Vec<SinglePass>,
    pub label: Option<ResolvedLabel>,
    pub unimplemented: Vec<&'static str>,
}

pub(super) fn resolve_class(
    p: ParsedClass,
    layer_name: &str,
    geom_kind: &str,
    class_item: Option<&str>,
    label_item: Option<&str>,
    symbols: &HashMap<String, SymbolDef>,
    composite: LayerComposite,
) -> ResolvedClass {
    let title = p.name.clone();
    let class_name = slugify(&p.name.unwrap_or_else(|| format!("class_l{}", p.class_line)));
    let style_prefix = if geom_kind == "polygon" { "poly" } else { geom_kind };
    let style_name = format!("{}_{}_{}", style_prefix, slugify(layer_name), class_name);

    // layer-wide COMPOSITE OPACITY composes multiplicatively with any
    // per-pass STYLE.OPACITY; absent pass opacity defaults to 1.0. COMPOSITE
    // COMPOP sets the per-pass blend_mode (no per-STYLE COMPOP in mapfile, so
    // no composition rule is needed).
    let passes: Vec<SinglePass> = p
        .styles
        .iter()
        .map(|sb| {
            let mut pass = style_block_to_pass(sb, symbols);
            if let Some(layer_op) = composite.opacity {
                let pass_op = pass.opacity.unwrap_or(1.0);
                pass.opacity = Some((layer_op * pass_op).clamp(0.0, 1.0));
            }
            if let Some(bm) = composite.blend_mode {
                pass.blend_mode = Some(bm);
            }
            pass
        })
        .collect();

    let when = resolve_when(p.expression, class_item, title.as_deref(), layer_name, p.class_line);

    let label = p
        .label
        .map(|pl| resolve_label(pl, &class_label_style_name(layer_name, &class_name), label_item));

    let mut unimplemented: Vec<&'static str> = Vec::new();
    for sb in &p.styles {
        for u in &sb.unimplemented {
            if !unimplemented.contains(u) {
                unimplemented.push(*u);
            }
        }
    }
    for pass in &passes {
        for u in &pass.unimplemented {
            if !unimplemented.contains(u) {
                unimplemented.push(*u);
            }
        }
    }

    ResolvedClass {
        class_name,
        title,
        when,
        min_scale_denom: p.min_scale_denom,
        max_scale_denom: p.max_scale_denom,
        style_type: geom_kind.to_string(),
        style_name,
        passes,
        label,
        unimplemented,
    }
}

/// reconcile a class's EXPRESSION shape with the layer's CLASSITEM.
///
/// `BareLiteral`, `Set`, and `Range` are CLASSITEM-relative by construction -
/// they pick up the column at this point. `Regex` is also CLASSITEM-relative
/// but yields `false` (mars_expr has no regex AST); the raw pattern is
/// surfaced via stderr. `Predicate` is self-contained and passes through.
/// `None` falls back to the CLASS NAME / CLASSITEM expansion that has always
/// existed for un-EXPRESSION'd classes.
///
/// untranslatable expressions emit `when: "false"` so the generated YAML
/// always parses cleanly; the human-readable signal is the stderr `warn!`
/// (counted by the CLI's `--strict` gate).
fn resolve_when(
    expression: Option<ParsedExpression>,
    class_item: Option<&str>,
    title: Option<&str>,
    layer_name: &str,
    class_line: usize,
) -> Option<String> {
    match expression {
        Some(ParsedExpression::Predicate(s)) => Some(s),
        Some(ParsedExpression::BareLiteral(lit)) => match class_item {
            Some(ci) => Some(format!("{ci} = {lit}")),
            None => {
                warn!(
                    layer = %layer_name,
                    line = class_line,
                    literal = %lit,
                    "CLASS EXPRESSION literal without CLASSITEM; emitting when:false",
                );
                Some("false".to_string())
            }
        },
        Some(ParsedExpression::Set(lits)) => match (class_item, lits.is_empty()) {
            (Some(ci), false) => Some(format_in(ci, &lits)),
            (Some(_), true) => {
                warn!(layer = %layer_name, line = class_line, "CLASS EXPRESSION empty set; emitting when:false");
                Some("false".to_string())
            }
            (None, _) => {
                warn!(
                    layer = %layer_name,
                    line = class_line,
                    "CLASS EXPRESSION set without CLASSITEM; emitting when:false",
                );
                Some("false".to_string())
            }
        },
        Some(ParsedExpression::Regex {
            pattern,
            case_insensitive,
        }) => match class_item {
            Some(ci) => {
                let op = if case_insensitive { "~*" } else { "~" };
                Some(format!("{ci} {op} '{}'", pattern.replace('\'', "''")))
            }
            None => {
                warn!(
                    layer = %layer_name,
                    line = class_line,
                    pattern = %pattern,
                    "CLASS EXPRESSION regex without CLASSITEM; emitting when:false",
                );
                Some("false".to_string())
            }
        },
        Some(ParsedExpression::Range { lo, hi }) => match class_item {
            Some(ci) => Some(format_range(ci, &lo, hi.as_ref())),
            None => {
                let suffix = hi.as_ref().map(|h| format!("{h}")).unwrap_or_default();
                warn!(
                    layer = %layer_name,
                    line = class_line,
                    range = format!("{lo}-{suffix}"),
                    "CLASS EXPRESSION range without CLASSITEM; emitting when:false",
                );
                Some("false".to_string())
            }
        },
        Some(ParsedExpression::Todo(raw)) => {
            warn!(layer = %layer_name, line = class_line, raw = %raw, "CLASS EXPRESSION not translatable; emitting when:false");
            Some("false".to_string())
        }
        None => match (class_item, title) {
            (Some(item), Some(value)) => Some(format!("{item} = '{}'", value.replace('\'', "''"))),
            _ => None,
        },
    }
}

fn format_in(column: &str, lits: &[mars_expr::Literal]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(column.len() + 8 + lits.len() * 6);
    let _ = write!(s, "{column} IN (");
    for (i, lit) in lits.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        let _ = write!(s, "{lit}");
    }
    s.push(')');
    s
}

fn format_range(column: &str, lo: &mars_expr::Literal, hi: Option<&mars_expr::Literal>) -> String {
    match hi {
        Some(hi) => format!("({column} >= {lo} AND {column} <= {hi})"),
        None => format!("{column} >= {lo}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use mars_expr::Literal;

    #[test]
    fn regex_with_classitem_emits_classitem_aware_predicate() {
        let w = resolve_when(
            Some(ParsedExpression::Regex {
                pattern: "^A".into(),
                case_insensitive: false,
            }),
            Some("rtt"),
            None,
            "layer",
            1,
        );
        assert_eq!(w.as_deref(), Some("rtt ~ '^A'"));
    }

    #[test]
    fn regex_without_classitem_emits_false() {
        let w = resolve_when(
            Some(ParsedExpression::Regex {
                pattern: "foo".into(),
                case_insensitive: false,
            }),
            None,
            None,
            "layer",
            1,
        );
        // untranslatable -> when:false so the YAML still parses; the raw
        // pattern is surfaced via the stderr `warn!` channel.
        assert_eq!(w.as_deref(), Some("false"));
    }

    #[test]
    fn regex_pattern_with_quote_is_doubled() {
        let w = resolve_when(
            Some(ParsedExpression::Regex {
                pattern: "o'brien".into(),
                case_insensitive: false,
            }),
            Some("name"),
            None,
            "layer",
            1,
        );
        assert_eq!(w.as_deref(), Some("name ~ 'o''brien'"));
    }

    #[test]
    fn regex_case_insensitive_lifts_to_tilde_star() {
        let w = resolve_when(
            Some(ParsedExpression::Regex {
                pattern: "highway".into(),
                case_insensitive: true,
            }),
            Some("kind"),
            None,
            "layer",
            1,
        );
        assert_eq!(w.as_deref(), Some("kind ~* 'highway'"));
    }

    #[test]
    fn closed_range_with_classitem_emits_bounded_predicate() {
        let w = resolve_when(
            Some(ParsedExpression::Range {
                lo: Literal::Int(2),
                hi: Some(Literal::Int(12)),
            }),
            Some("rtt"),
            None,
            "layer",
            1,
        );
        let s = w.unwrap();
        assert_eq!(s, "(rtt >= 2 AND rtt <= 12)");
        // round-trips through mars_expr
        mars_expr::parse(&s).unwrap();
    }

    #[test]
    fn open_upper_range_with_classitem_emits_lower_bound_only() {
        let w = resolve_when(
            Some(ParsedExpression::Range {
                lo: Literal::Int(100),
                hi: None,
            }),
            Some("rtt"),
            None,
            "layer",
            1,
        );
        let s = w.unwrap();
        assert_eq!(s, "rtt >= 100");
        mars_expr::parse(&s).unwrap();
    }

    #[test]
    fn range_without_classitem_emits_false() {
        let w = resolve_when(
            Some(ParsedExpression::Range {
                lo: Literal::Int(2),
                hi: Some(Literal::Int(12)),
            }),
            None,
            None,
            "layer",
            1,
        );
        // untranslatable -> when:false; the raw range is surfaced via warn!.
        assert_eq!(w.as_deref(), Some("false"));
    }

    #[test]
    fn mixed_range_round_trips() {
        let w = resolve_when(
            Some(ParsedExpression::Range {
                lo: Literal::Int(0),
                hi: Some(Literal::Float(2.5)),
            }),
            Some("rtt"),
            None,
            "layer",
            1,
        );
        let s = w.unwrap();
        assert_eq!(s, "(rtt >= 0 AND rtt <= 2.5)");
        mars_expr::parse(&s).unwrap();
    }
}
