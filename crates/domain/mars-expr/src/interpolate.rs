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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct Row(BTreeMap<&'static str, Literal>);
    impl AttributeAccess for Row {
        fn get(&self, name: &str) -> Option<Literal> {
            self.0.get(name).cloned()
        }
    }

    fn row(pairs: &[(&'static str, Literal)]) -> Row {
        Row(pairs.iter().cloned().collect())
    }

    #[test]
    fn empty_template_yields_no_segments() {
        let t = parse_template("").unwrap();
        assert!(t.segments.is_empty());
        assert_eq!(eval_template(&t, &row(&[])).unwrap(), "");
    }

    #[test]
    fn pure_literal() {
        let t = parse_template("hello world").unwrap();
        assert_eq!(t.segments, vec![Segment::Literal("hello world".into())]);
        assert_eq!(eval_template(&t, &row(&[])).unwrap(), "hello world");
    }

    #[test]
    fn pure_substitution() {
        let t = parse_template("{name}").unwrap();
        assert_eq!(t.segments, vec![Segment::Ident("name".into())]);
        let r = row(&[("name", Literal::String("alice".into()))]);
        assert_eq!(eval_template(&t, &r).unwrap(), "alice");
    }

    #[test]
    fn mixed_segments() {
        let t = parse_template("[{kind}] {name} ({n})").unwrap();
        let r = row(&[
            ("kind", Literal::String("road".into())),
            ("name", Literal::String("Main".into())),
            ("n", Literal::Int(42)),
        ]);
        assert_eq!(eval_template(&t, &r).unwrap(), "[road] Main (42)");
    }

    #[test]
    fn missing_attr_renders_empty() {
        let t = parse_template("name={name}, age={age}").unwrap();
        let r = row(&[("name", Literal::String("ada".into()))]);
        assert_eq!(eval_template(&t, &r).unwrap(), "name=ada, age=");
    }

    #[test]
    fn null_attr_renders_empty() {
        let t = parse_template("{x}").unwrap();
        let r = row(&[("x", Literal::Null)]);
        assert_eq!(eval_template(&t, &r).unwrap(), "");
    }

    #[test]
    fn rejects_unmatched_brace() {
        assert!(parse_template("{name").is_err());
    }

    #[test]
    fn rejects_empty_placeholder() {
        assert!(parse_template("a{}b").is_err());
    }

    #[test]
    fn rejects_invalid_ident() {
        assert!(parse_template("{1bad}").is_err());
        assert!(parse_template("{na me}").is_err());
        assert!(parse_template("{na-me}").is_err());
    }

    #[test]
    fn rejects_stray_close() {
        assert!(parse_template("a}b").is_err());
    }

    #[test]
    fn underscored_idents_ok() {
        let t = parse_template("{_x}{x_2}").unwrap();
        let r = row(&[
            ("_x", Literal::String("a".into())),
            ("x_2", Literal::String("b".into())),
        ]);
        assert_eq!(eval_template(&t, &r).unwrap(), "ab");
    }
}
