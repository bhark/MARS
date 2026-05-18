#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::collections::HashMap;

struct Map(HashMap<String, Literal>);
impl AttributeAccess for Map {
    fn get(&self, name: &str) -> Option<Literal> {
        self.0.get(name).cloned()
    }
}
fn attrs(pairs: &[(&str, Literal)]) -> Map {
    Map(pairs.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect())
}

#[test]
fn parse_empty_errors() {
    assert!(matches!(parse(""), Err(ExprError::Parse(_))));
    assert!(matches!(parse("   "), Err(ExprError::Parse(_))));
}

#[test]
fn parse_basic_cmp_and_and() {
    let e = parse("ttype = 'forest' AND area >= 1000").unwrap();
    // expect a flattened AND of two cmps
    match e {
        Expr::Logic { op: LogicOp::And, args } => {
            assert_eq!(args.len(), 2);
            assert!(matches!(args[0], Expr::Cmp { op: CmpOp::Eq, .. }));
            assert!(matches!(args[1], Expr::Cmp { op: CmpOp::Ge, .. }));
        }
        _ => panic!("expected AND"),
    }
}

#[test]
fn parse_like() {
    let e = parse("name LIKE 'foo%'").unwrap();
    assert!(matches!(e, Expr::Like { .. }));
}

#[test]
fn parse_in_list() {
    let e = parse("kind IN ('a','b','c')").unwrap();
    match e {
        Expr::In { list, .. } => assert_eq!(list.len(), 3),
        _ => panic!("expected IN"),
    }
}

#[test]
fn parse_not_grouped() {
    let e = parse("NOT (a = 1 OR b = 2)").unwrap();
    assert!(matches!(e, Expr::Not(_)));
}

#[test]
fn parse_is_not_null() {
    let e = parse("name IS NOT NULL").unwrap();
    assert!(matches!(e, Expr::IsNotNull(_)));
}

#[test]
fn parse_string_with_escaped_quote() {
    let e = parse("'with ''quote'' inside' = name").unwrap();
    match e {
        Expr::Cmp { lhs, .. } => match *lhs {
            Expr::Literal(Literal::String(s)) => {
                assert_eq!(s, "with 'quote' inside");
            }
            _ => panic!("expected string lhs"),
        },
        _ => panic!("expected cmp"),
    }
}

#[test]
fn reject_arithmetic() {
    assert!(matches!(parse("a + 1"), Err(ExprError::Parse(_))));
    assert!(matches!(parse("a - 1"), Err(ExprError::Parse(_))));
    assert!(matches!(parse("a * 1"), Err(ExprError::Parse(_))));
    assert!(matches!(parse("a / 1"), Err(ExprError::Parse(_))));
}

#[test]
fn reject_function_call() {
    assert!(matches!(parse("func(x)"), Err(ExprError::Parse(_))));
}

#[test]
fn reject_double_quoted() {
    assert!(matches!(parse("\"quoted\""), Err(ExprError::Parse(_))));
}

#[test]
fn parse_regex_case_sensitive() {
    let e = parse("name ~ '^foo'").unwrap();
    match e {
        Expr::Regex {
            pattern,
            case_insensitive,
            ..
        } => {
            assert_eq!(pattern, "^foo");
            assert!(!case_insensitive);
        }
        _ => panic!("expected Regex"),
    }
}

#[test]
fn parse_regex_case_insensitive() {
    let e = parse("name ~* 'foo'").unwrap();
    match e {
        Expr::Regex {
            pattern,
            case_insensitive,
            ..
        } => {
            assert_eq!(pattern, "foo");
            assert!(case_insensitive);
        }
        _ => panic!("expected Regex"),
    }
}

#[test]
fn parse_regex_requires_string_pattern() {
    // bareword / numeric pattern is a parse error - only quoted strings.
    assert!(matches!(parse("name ~ foo"), Err(ExprError::Parse(_))));
    assert!(matches!(parse("name ~ 42"), Err(ExprError::Parse(_))));
}

#[test]
fn eval_regex_case_sensitive() {
    let a = attrs(&[("s", Literal::String("Foobar".into()))]);
    assert_eq!(eval(&parse("s ~ '^Foo'").unwrap(), &a).unwrap(), Literal::Bool(true));
    // case-sensitive: lowercase anchor should not match
    assert_eq!(eval(&parse("s ~ '^foo'").unwrap(), &a).unwrap(), Literal::Bool(false));
}

#[test]
fn eval_regex_case_insensitive() {
    let a = attrs(&[("s", Literal::String("Foobar".into()))]);
    assert_eq!(eval(&parse("s ~* '^foo'").unwrap(), &a).unwrap(), Literal::Bool(true));
    assert_eq!(eval(&parse("s ~* 'BAR$'").unwrap(), &a).unwrap(), Literal::Bool(true));
}

#[test]
fn eval_regex_unanchored_substring() {
    let a = attrs(&[("s", Literal::String("hello world".into()))]);
    assert_eq!(eval(&parse("s ~ 'wor'").unwrap(), &a).unwrap(), Literal::Bool(true));
    assert_eq!(eval(&parse("s ~ 'xyz'").unwrap(), &a).unwrap(), Literal::Bool(false));
}

#[test]
fn eval_regex_escaped_metachars() {
    let a = attrs(&[("s", Literal::String("a.b".into()))]);
    // escaped '.' only matches literal dot
    assert_eq!(eval(&parse("s ~ '^a\\.b$'").unwrap(), &a).unwrap(), Literal::Bool(true));
    // unescaped '.' would also match here, so use a value that distinguishes
    let a2 = attrs(&[("s", Literal::String("aXb".into()))]);
    assert_eq!(
        eval(&parse("s ~ '^a\\.b$'").unwrap(), &a2).unwrap(),
        Literal::Bool(false)
    );
    assert_eq!(eval(&parse("s ~ '^a.b$'").unwrap(), &a2).unwrap(), Literal::Bool(true));
}

#[test]
fn eval_regex_null_propagates() {
    let a = attrs(&[("s", Literal::Null)]);
    assert!(matches!(eval(&parse("s ~ 'foo'").unwrap(), &a).unwrap(), Literal::Null));
}

#[test]
fn eval_regex_invalid_pattern() {
    let a = attrs(&[("s", Literal::String("x".into()))]);
    let r = eval(&parse("s ~ '(unclosed'").unwrap(), &a);
    assert!(matches!(r, Err(ExprError::InvalidRegex { .. })));
}

#[test]
fn regex_roundtrip_via_display() {
    for s in ["name ~ '^foo'", "name ~* 'bar$'", "n ~ 'a\\.b'"] {
        let e1 = parse(s).unwrap();
        let s2 = format!("{e1}");
        let e2 = parse(&s2).unwrap_or_else(|err| panic!("reparse `{s2}` failed: {err}"));
        assert_eq!(e1, e2, "roundtrip mismatch for `{s}` -> `{s2}`");
    }
}

#[test]
fn roundtrip_via_display() {
    let inputs = [
        "ttype = 'forest' AND area >= 1000",
        "name LIKE 'foo%'",
        "kind IN ('a', 'b', 'c')",
        "NOT (a = 1 OR b = 2)",
        "name IS NOT NULL",
        "'with ''quote'' inside' = name",
        "a = 1 OR b = 2 OR c = 3",
        "a = 1 AND (b = 2 OR c = 3)",
    ];
    for s in inputs {
        let e1 = parse(s).unwrap();
        let s2 = format!("{e1}");
        let e2 = parse(&s2).unwrap_or_else(|err| panic!("reparse `{s2}` failed: {err}"));
        assert_eq!(e1, e2, "roundtrip mismatch for `{s}` -> `{s2}`");
    }
}

#[test]
fn parser_rejects_eq_null() {
    // `= NULL` / `!= NULL` are never-true in SQL; the parser steers users
    // to IS NULL / IS NOT NULL instead.
    assert!(matches!(parse("a = NULL"), Err(ExprError::Parse(_))));
    assert!(matches!(parse("a != NULL"), Err(ExprError::Parse(_))));
}

#[test]
fn eval_three_valued_with_null_attr() {
    // building Cmp(Eq, ident, Null) directly bypasses the parser; eval
    // still returns NULL per three-valued logic.
    let e = Expr::Cmp {
        op: CmpOp::Eq,
        lhs: Box::new(Expr::Ident("a".into())),
        rhs: Box::new(Expr::Literal(Literal::Null)),
    };
    let r = eval(&e, &attrs(&[("a", Literal::Int(1))])).unwrap();
    assert!(matches!(r, Literal::Null));
}

#[test]
fn eval_and_truth_table() {
    let a = attrs(&[
        ("t", Literal::Bool(true)),
        ("f", Literal::Bool(false)),
        ("n", Literal::Null),
    ]);
    // null AND true = null
    let e = parse("n = n AND t = t").unwrap();
    // n = n is null; t = t is true
    assert!(matches!(eval(&e, &a).unwrap(), Literal::Null));
    // null AND false = false
    let e = parse("n = n AND f = t").unwrap();
    assert_eq!(eval(&e, &a).unwrap(), Literal::Bool(false));
}

#[test]
fn eval_or_truth_table() {
    let a = attrs(&[
        ("t", Literal::Bool(true)),
        ("f", Literal::Bool(false)),
        ("n", Literal::Null),
    ]);
    // null OR true = true
    let e = parse("n = n OR t = t").unwrap();
    assert_eq!(eval(&e, &a).unwrap(), Literal::Bool(true));
    // null OR false = null
    let e = parse("n = n OR f = t").unwrap();
    assert!(matches!(eval(&e, &a).unwrap(), Literal::Null));
}

#[test]
fn eval_is_null_variants() {
    // missing ident → eval error, not "is null"
    let e = parse("missing IS NULL").unwrap();
    assert!(matches!(eval(&e, &attrs(&[])), Err(ExprError::UnknownIdent(_))));
    // explicit Null → IS NULL true
    let e = parse("x IS NULL").unwrap();
    assert_eq!(eval(&e, &attrs(&[("x", Literal::Null)])).unwrap(), Literal::Bool(true));
    // set value → IS NULL false
    assert_eq!(
        eval(&e, &attrs(&[("x", Literal::Int(7))])).unwrap(),
        Literal::Bool(false)
    );
}

#[test]
fn eval_like_wildcards() {
    let a = attrs(&[("s", Literal::String("foobar".into()))]);
    assert_eq!(eval(&parse("s LIKE 'foo%'").unwrap(), &a).unwrap(), Literal::Bool(true));
    assert_eq!(eval(&parse("s LIKE '%bar'").unwrap(), &a).unwrap(), Literal::Bool(true));
    assert_eq!(
        eval(&parse("s LIKE 'f__bar'").unwrap(), &a).unwrap(),
        Literal::Bool(true)
    );
    assert_eq!(
        eval(&parse("s LIKE 'baz%'").unwrap(), &a).unwrap(),
        Literal::Bool(false)
    );
    assert_eq!(eval(&parse("s LIKE '%'").unwrap(), &a).unwrap(), Literal::Bool(true));
    assert_eq!(eval(&parse("s LIKE '_'").unwrap(), &a).unwrap(), Literal::Bool(false));
}

#[test]
fn eval_empty_in_list() {
    let e = parse("x IN ()").unwrap();
    assert_eq!(
        eval(&e, &attrs(&[("x", Literal::Int(1))])).unwrap(),
        Literal::Bool(false)
    );
}

#[test]
fn eval_in_match() {
    let e = parse("x IN (1, 2, 3)").unwrap();
    assert_eq!(
        eval(&e, &attrs(&[("x", Literal::Int(2))])).unwrap(),
        Literal::Bool(true)
    );
    assert_eq!(
        eval(&e, &attrs(&[("x", Literal::Int(9))])).unwrap(),
        Literal::Bool(false)
    );
}

#[test]
fn eval_unknown_ident() {
    let e = parse("missing = 1").unwrap();
    assert!(matches!(eval(&e, &attrs(&[])), Err(ExprError::UnknownIdent(n)) if n == "missing"));
}

#[test]
fn parse_string_with_utf8_multibyte() {
    // the config explicitly uses 'Foreløbig'; byte-as-char would mangle 'ø'.
    let e = parse("status = 'Foreløbig'").unwrap();
    match e {
        Expr::Cmp { rhs, .. } => match *rhs {
            Expr::Literal(Literal::String(s)) => assert_eq!(s, "Foreløbig"),
            _ => panic!("expected string"),
        },
        _ => panic!("expected cmp"),
    }
}

#[test]
fn parse_rejects_deeply_nested_parens() {
    let s = format!("{}x{}", "(".repeat(100), ")".repeat(100));
    assert!(matches!(parse(&s), Err(ExprError::TooDeep { .. })));
}

#[test]
fn parse_rejects_deeply_nested_not() {
    let mut s = String::new();
    for _ in 0..100 {
        s.push_str("NOT ");
    }
    s.push_str("x = 1");
    assert!(matches!(parse(&s), Err(ExprError::TooDeep { .. })));
}

#[test]
fn parse_rejects_non_finite_float() {
    // tokenizer never produces NaN literally (no 'NaN' keyword), but
    // overflow into Inf during parse must be rejected.
    assert!(matches!(parse("x = 1e400"), Err(ExprError::Parse(_))));
}

#[test]
fn float_display_roundtrips_large_value() {
    // 1e20 must format with a decimal point or exponent so reparse stays Float.
    let lit = Literal::Float(1e20);
    let s = format!("{lit}");
    // must contain '.' or 'e' so reparse stays Float
    assert!(s.contains('.') || s.contains('e') || s.contains('E'), "got {s}");
    let e = parse(&format!("x = {s}")).unwrap();
    match e {
        Expr::Cmp { rhs, .. } => match *rhs {
            Expr::Literal(Literal::Float(_)) => {}
            other => panic!("expected Float, got {other:?}"),
        },
        _ => panic!("expected cmp"),
    }
}

#[test]
fn eval_int_float_promotion() {
    let e = parse("x >= 10").unwrap();
    assert_eq!(
        eval(&e, &attrs(&[("x", Literal::Float(10.5))])).unwrap(),
        Literal::Bool(true)
    );
}
