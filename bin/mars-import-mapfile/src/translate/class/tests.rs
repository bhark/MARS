#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

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
        ParsedExpression::Regex {
            pattern,
            case_insensitive,
        } => {
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
