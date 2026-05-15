//! CLASS block parser. Walk tokens, accumulate a [`ParsedClass`] bag of
//! `Option` fields and per-class [`StyleBlock`]s. No defaulting, no emit -
//! defaults live in [`super::resolved`]; emit lives in [`super::emit`].

use mars_expr::{Expr, Literal};
use tracing::warn;

use crate::directive::ClassDirective;
use crate::expression::{parse_mapfile_expression, parse_set_literal};
use crate::scanner::{Token, block_range, is_block_opener};

use super::is_unsupported;
use super::label::{ParsedLabel, parse_label};
use super::style_block::{StyleBlock, parse_style_block};

#[derive(Debug, Default)]
pub(crate) struct ParsedClass {
    pub class_line: usize,
    pub name: Option<String>,
    pub expression: Option<ParsedExpression>,
    pub styles: Vec<StyleBlock>,
    pub min_scale_denom: Option<u64>,
    pub max_scale_denom: Option<u64>,
    pub label: Option<ParsedLabel>,
}

/// shape of a parsed CLASS-level EXPRESSION. some forms are self-contained
/// predicates; others need CLASSITEM context applied at resolve time. the
/// scanner strips quotes from args, so a bareword and a quoted literal are
/// indistinguishable here - both lower to `BareLiteral`.
#[derive(Debug)]
pub(crate) enum ParsedExpression {
    // complete predicate ready to emit verbatim
    Predicate(String),
    // a single value (literal or bareword) needing CLASSITEM equality wrapping
    BareLiteral(Literal),
    // `{v1,v2,...}` set form needing CLASSITEM IN wrapping
    Set(Vec<Literal>),
    // unparsable; raw text preserved for the TODO comment
    Todo(String),
}

pub(crate) fn parse_class(body: &[Token], class_line: usize) -> ParsedClass {
    let mut p = ParsedClass {
        class_line,
        ..Default::default()
    };

    let mut i = 0;
    while i < body.len() {
        let t = &body[i];
        match ClassDirective::from_token(t, is_unsupported) {
            ClassDirective::Name(t) if p.name.is_none() => p.name = t.args.first().cloned(),
            ClassDirective::MinScaleDenom(t) => {
                if let Some(n) = parse_class_scale_denom(t) {
                    p.min_scale_denom = Some(n);
                }
            }
            ClassDirective::MaxScaleDenom(t) => {
                if let Some(n) = parse_class_scale_denom(t) {
                    p.max_scale_denom = Some(n);
                }
            }
            ClassDirective::Expression(t) => {
                p.expression = Some(parse_class_expression(t));
            }
            ClassDirective::Style => {
                if let Some(r) = block_range(body, i) {
                    p.styles.push(parse_style_block(&body[r.start + 1..r.end - 1]));
                    i = r.end;
                    continue;
                }
            }
            ClassDirective::Label(_t) => {
                if let Some(r) = block_range(body, i) {
                    // last LABEL wins on repeat, mirroring the layer-level path.
                    p.label = Some(parse_label(&body[r.start + 1..r.end - 1]));
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

    p
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

/// classify a CLASS EXPRESSION token into one of the four parsed shapes.
/// the scanner strips surrounding double quotes, so `EXPRESSION "lit"` and
/// `EXPRESSION lit` arrive identically as a single arg - both lower to
/// `BareLiteral` here and pick up their column at resolve time.
fn parse_class_expression(t: &Token) -> ParsedExpression {
    let joined = t.args.join(" ");
    let trimmed = joined.trim();

    if trimmed.starts_with('{') {
        return match parse_set_literal(trimmed, t.line) {
            Ok(lits) => ParsedExpression::Set(lits),
            Err(e) => {
                warn!(line = t.line, error = %e, "could not parse EXPRESSION set");
                ParsedExpression::Todo(joined)
            }
        };
    }

    match parse_mapfile_expression(trimmed, t.line) {
        Ok(Expr::Literal(lit)) => ParsedExpression::BareLiteral(lit),
        Ok(expr) => ParsedExpression::Predicate(format!("{expr}")),
        Err(e) => {
            // single-arg fallback: the scanner strips quotes, so a quoted
            // string and an unquoted bareword both arrive as one arg with
            // no expression-y characters. treat as a literal value (the
            // resolver decides whether to wrap with CLASSITEM). regex form
            // `/.../` is held back so it still surfaces as a TODO.
            if t.args.len() == 1 && !t.args[0].starts_with('/') {
                ParsedExpression::BareLiteral(Literal::String(t.args[0].clone()))
            } else {
                warn!(line = t.line, error = %e, "could not parse EXPRESSION");
                ParsedExpression::Todo(joined)
            }
        }
    }
}
