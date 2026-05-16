//! CLASS block parser. Walk tokens, accumulate a [`ParsedClass`] bag of
//! `Option` fields and per-class [`StyleBlock`]s. No defaulting, no emit -
//! defaults live in [`super::resolved`]; emit lives in [`super::emit`].

use mars_expr::{Expr, Literal};
use tracing::warn;

use crate::directive::ClassDirective;
use crate::expression::{number_or_string, parse_mapfile_expression, parse_set_literal};
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
    // `/pat/` or `/pat/i` regex form needing CLASSITEM regex-match wrapping.
    // case_insensitive carries the optional trailing `i` flag.
    Regex { pattern: String, case_insensitive: bool },
    // `lo-hi` / `lo-` numeric range shorthand over a numeric CLASSITEM. lifts
    // through the resolver as `(ci >= lo AND ci <= hi)` (or `ci >= lo` when
    // the upper bound is open).
    Range { lo: Literal, hi: Option<Literal> },
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

/// classify a CLASS EXPRESSION token into one of the parsed shapes.
/// the scanner strips surrounding double quotes, so `EXPRESSION "lit"` and
/// `EXPRESSION lit` arrive identically as a single arg - both lower to
/// `BareLiteral` here and pick up their column at resolve time. regex
/// (`/.../`) and range (`lo-hi` / `lo-`) shorthand are detected before the
/// general expression parser since the parser would otherwise reject the
/// leading `/` or fuse the hyphen into a number token.
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

    // mapfile `/pat/` or `/pat/i` -> CLASSITEM-relative regex match (forwarded
    // to the resolver, which emits `ci ~ 'pat'` or `ci ~* 'pat'`).
    if let Some(rx) = strip_regex_form(t.args.as_slice()) {
        return ParsedExpression::Regex {
            pattern: rx.pattern,
            case_insensitive: rx.case_insensitive,
        };
    }

    if t.args.len() == 1
        && let Some((lo, hi)) = parse_range_shorthand(&t.args[0])
    {
        return ParsedExpression::Range { lo, hi };
    }

    match parse_mapfile_expression(trimmed, t.line) {
        Ok(Expr::Literal(lit)) => ParsedExpression::BareLiteral(lit),
        Ok(expr) => ParsedExpression::Predicate(format!("{expr}")),
        Err(e) => {
            // single-arg fallback: the scanner strips quotes, so a quoted
            // string and an unquoted bareword both arrive as one arg with
            // no expression-y characters. treat as a literal value (the
            // resolver decides whether to wrap with CLASSITEM). malformed
            // `/...` shapes still surface as TODO via the regex guard above
            // returning None.
            if t.args.len() == 1 && !t.args[0].starts_with('/') {
                ParsedExpression::BareLiteral(Literal::String(t.args[0].clone()))
            } else {
                warn!(line = t.line, error = %e, "could not parse EXPRESSION");
                ParsedExpression::Todo(joined)
            }
        }
    }
}

struct StrippedRegex {
    pattern: String,
    case_insensitive: bool,
}

/// recognise the mapfile `/pat/` or `/pat/i` regex form. expects the scanner
/// to have left the slashes intact on a single arg; multi-arg or anything not
/// wrapped in slashes returns None and falls through to the regular parse.
fn strip_regex_form(args: &[String]) -> Option<StrippedRegex> {
    if args.len() != 1 {
        return None;
    }
    let raw = &args[0];
    if !raw.starts_with('/') {
        return None;
    }
    let body = &raw[1..];
    let (pattern, case_insensitive) = if let Some(p) = body.strip_suffix("/i") {
        (p, true)
    } else if let Some(p) = body.strip_suffix('/') {
        (p, false)
    } else {
        return None;
    };
    if pattern.is_empty() || pattern.contains('/') {
        return None;
    }
    Some(StrippedRegex {
        pattern: pattern.to_string(),
        case_insensitive,
    })
}

// recognise `lo-hi` and `lo-` where lo/hi are unsigned decimal numbers. no
// leading sign, no scientific notation - intentionally tight so we don't
// steal meaning from negative-literal class predicates like `-12`.
fn parse_range_shorthand(arg: &str) -> Option<(Literal, Option<Literal>)> {
    if !arg.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let dash = arg.find('-')?;
    let (lo_str, rest) = arg.split_at(dash);
    let hi_str = &rest[1..];
    if !is_unsigned_decimal(lo_str) {
        return None;
    }
    let hi = if hi_str.is_empty() {
        None
    } else if is_unsigned_decimal(hi_str) {
        Some(number_or_string(hi_str))
    } else {
        return None;
    };
    Some((number_or_string(lo_str), hi))
}

fn is_unsigned_decimal(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut seen_dot = false;
    for c in s.chars() {
        if c == '.' {
            if seen_dot {
                return false;
            }
            seen_dot = true;
        } else if !c.is_ascii_digit() {
            return false;
        }
    }
    // reject lone `.`
    s.chars().any(|c| c.is_ascii_digit())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn tok(arg: &str) -> Token {
        Token {
            line: 1,
            keyword: "EXPRESSION".to_string(),
            args: vec![arg.to_string()],
        }
    }

    fn one_arg(s: &str) -> Vec<String> {
        vec![s.to_string()]
    }

    #[test]
    fn regex_shorthand_lifts_pattern() {
        match parse_class_expression(&tok("/Hovedrute/")) {
            ParsedExpression::Regex { pattern, case_insensitive } => {
                assert_eq!(pattern, "Hovedrute");
                assert!(!case_insensitive);
            }
            other => panic!("expected Regex, got {other:?}"),
        }
    }

    #[test]
    fn regex_empty_body_falls_to_todo() {
        assert!(matches!(parse_class_expression(&tok("//")), ParsedExpression::Todo(_)));
    }

    #[test]
    fn regex_inner_slash_falls_to_todo() {
        // never silently truncate /a/b/ to "a/b"; surface as TODO instead.
        assert!(matches!(
            parse_class_expression(&tok("/foo/bar/")),
            ParsedExpression::Todo(_)
        ));
    }

    #[test]
    fn strip_regex_basic() {
        let r = strip_regex_form(&one_arg("/foo.*/")).unwrap();
        assert_eq!(r.pattern, "foo.*");
        assert!(!r.case_insensitive);
    }

    #[test]
    fn strip_regex_case_insensitive_flag() {
        let r = strip_regex_form(&one_arg("/foo.*/i")).unwrap();
        assert_eq!(r.pattern, "foo.*");
        assert!(r.case_insensitive);
    }

    #[test]
    fn strip_regex_rejects_non_slash_forms() {
        assert!(strip_regex_form(&one_arg("foo")).is_none());
        assert!(strip_regex_form(&one_arg("/incomplete")).is_none());
        assert!(strip_regex_form(&one_arg("//")).is_none());
        assert!(strip_regex_form(&[]).is_none());
        assert!(strip_regex_form(&[String::from("/a/"), String::from("extra")]).is_none());
    }

    #[test]
    fn range_closed_int_pair() {
        match parse_class_expression(&tok("2-12")) {
            ParsedExpression::Range { lo, hi } => {
                assert_eq!(lo, Literal::Int(2));
                assert_eq!(hi, Some(Literal::Int(12)));
            }
            other => panic!("expected Range, got {other:?}"),
        }
    }

    #[test]
    fn range_open_upper_bound() {
        match parse_class_expression(&tok("12-")) {
            ParsedExpression::Range { lo, hi } => {
                assert_eq!(lo, Literal::Int(12));
                assert_eq!(hi, None);
            }
            other => panic!("expected Range, got {other:?}"),
        }
    }

    #[test]
    fn range_mixed_int_float() {
        match parse_class_expression(&tok("0-2.5")) {
            ParsedExpression::Range { lo, hi } => {
                assert_eq!(lo, Literal::Int(0));
                assert_eq!(hi, Some(Literal::Float(2.5)));
            }
            other => panic!("expected Range, got {other:?}"),
        }
    }

    #[test]
    fn negative_literal_is_not_a_range() {
        // `-12` is a literal -12, never an upper-bound half-open range.
        match parse_class_expression(&tok("-12")) {
            ParsedExpression::BareLiteral(Literal::Int(-12)) => {}
            other => panic!("expected BareLiteral(Int(-12)), got {other:?}"),
        }
    }

    #[test]
    fn string_with_hyphen_is_not_a_range() {
        // leading non-digit disqualifies the range shorthand; falls back to
        // the single-arg bareword path.
        match parse_class_expression(&tok("foo-bar")) {
            ParsedExpression::BareLiteral(Literal::String(s)) => assert_eq!(s, "foo-bar"),
            other => panic!("expected BareLiteral(String), got {other:?}"),
        }
    }
}
