//! Text-template interpolation for label `text:` strings.
//!
//! A template is a sequence of literal segments and `{ident}` placeholders.
//! Identifier syntax matches the WHERE-clause lexer: ASCII alpha/underscore
//! followed by ASCII alnum/underscore. No escapes, no nested braces, no
//! quoting; if you need a literal `{`, do not put one in the template.
//!
//! At eval time, every placeholder is rendered via the shared
//! [`AttributeAccess`] trait. Missing or NULL attributes render as the empty
//! string. Forgiving label rendering is preferred over runtime
//! errors when a row is sparsely populated.

use crate::{AttributeAccess, ExprError, Literal};

#[derive(Debug, Clone, PartialEq)]
pub enum Segment {
    Literal(String),
    Ident(String),
}

/// Parsed template: ordered segments, ready to feed into [`eval_template`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Template {
    pub segments: Vec<Segment>,
}

/// Parse a template string into segments. Errors:
/// - unmatched `{`
/// - empty `{}` placeholder
/// - placeholder body that is not a valid identifier
/// - a stray `}` outside of a placeholder
pub fn parse_template(input: &str) -> Result<Template, ExprError> {
    let bytes = input.as_bytes();
    let mut segments = Vec::new();
    let mut lit_start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                if lit_start < i {
                    segments.push(Segment::Literal(input[lit_start..i].to_string()));
                }
                let body_start = i + 1;
                let close = input[body_start..]
                    .find('}')
                    .ok_or_else(|| ExprError::Parse(format!("unmatched '{{' at byte {i}")))?;
                let body_end = body_start + close;
                let body = &input[body_start..body_end];
                if body.is_empty() {
                    return Err(ExprError::Parse(format!("empty placeholder at byte {i}")));
                }
                if !is_ident(body) {
                    return Err(ExprError::Parse(format!(
                        "invalid identifier {body:?} in placeholder at byte {i}"
                    )));
                }
                segments.push(Segment::Ident(body.to_string()));
                i = body_end + 1;
                lit_start = i;
            }
            b'}' => {
                return Err(ExprError::Parse(format!("stray '}}' at byte {i}")));
            }
            _ => {
                i += 1;
            }
        }
    }
    if lit_start < bytes.len() {
        segments.push(Segment::Literal(input[lit_start..].to_string()));
    }
    Ok(Template { segments })
}

/// Render `template` against an attribute row. Missing or NULL attributes
/// render as the empty string.
pub fn eval_template(template: &Template, attrs: &dyn AttributeAccess) -> Result<String, ExprError> {
    let mut out = String::new();
    for seg in &template.segments {
        match seg {
            Segment::Literal(s) => out.push_str(s),
            Segment::Ident(name) => match attrs.get(name) {
                None | Some(Literal::Null) => {}
                Some(Literal::Bool(true)) => out.push_str("true"),
                Some(Literal::Bool(false)) => out.push_str("false"),
                Some(Literal::Int(n)) => {
                    use std::fmt::Write as _;
                    let _ = write!(out, "{n}");
                }
                Some(Literal::Float(v)) => {
                    use std::fmt::Write as _;
                    let _ = write!(out, "{v}");
                }
                Some(Literal::String(s)) => out.push_str(&s),
            },
        }
    }
    Ok(out)
}

fn is_ident(s: &str) -> bool {
    let mut it = s.bytes();
    let Some(first) = it.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    it.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod tests;
