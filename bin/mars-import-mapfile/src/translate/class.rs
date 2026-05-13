//! CLASS block parser. Walk tokens, accumulate a [`ParsedClass`] bag of
//! `Option` fields and per-class [`StyleBlock`]s. No defaulting, no emit -
//! defaults live in [`super::resolved`]; emit lives in [`super::emit`].

use tracing::warn;

use crate::directive::ClassDirective;
use crate::scanner::{Token, block_range, is_block_opener};

use super::is_unsupported;
use super::style_block::{StyleBlock, parse_style_block};

#[derive(Debug, Default)]
pub(crate) struct ParsedClass {
    pub class_line: usize,
    pub name: Option<String>,
    pub expression: Option<String>,
    pub styles: Vec<StyleBlock>,
    pub min_scale_denom: Option<u64>,
    pub max_scale_denom: Option<u64>,
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
                let joined = t.args.join(" ");
                match crate::expression::parse_mapfile_expression(&joined, t.line) {
                    Ok(expr) => {
                        p.expression = Some(format!("{expr}"));
                    }
                    Err(e) => {
                        warn!(line = t.line, error = %e, "could not parse EXPRESSION");
                        p.expression = Some(format!("# TODO: hand-translate: {joined}"));
                    }
                }
            }
            ClassDirective::Style => {
                if let Some(r) = block_range(body, i) {
                    p.styles.push(parse_style_block(&body[r.start + 1..r.end - 1]));
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
