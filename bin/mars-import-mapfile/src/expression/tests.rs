#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn attr_eq_string() {
    let e = parse_mapfile_expression("[bygningstype] = 'Drivhus'", 1).unwrap();
    assert_eq!(
        e,
        Expr::Cmp {
            op: CmpOp::Eq,
            lhs: Box::new(Expr::Ident("bygningstype".into())),
            rhs: Box::new(Expr::Literal(Literal::String("Drivhus".into()))),
        }
    );
}

#[test]
fn ne_and_eq() {
    let e = parse_mapfile_expression("[geometristatus] <> 'Foreløbig' AND [bygningstype] = 'Drivhus'", 1).unwrap();
    assert_eq!(
        e,
        Expr::Logic {
            op: LogicOp::And,
            args: vec![
                Expr::Cmp {
                    op: CmpOp::Ne,
                    lhs: Box::new(Expr::Ident("geometristatus".into())),
                    rhs: Box::new(Expr::Literal(Literal::String("Foreløbig".into()))),
                },
                Expr::Cmp {
                    op: CmpOp::Eq,
                    lhs: Box::new(Expr::Ident("bygningstype".into())),
                    rhs: Box::new(Expr::Literal(Literal::String("Drivhus".into()))),
                },
            ],
        }
    );
}

#[test]
fn in_list() {
    let e = parse_mapfile_expression("[vejkategori] IN ('Hovedrute', 'Stor vej')", 1).unwrap();
    assert_eq!(
        e,
        Expr::In {
            lhs: Box::new(Expr::Ident("vejkategori".into())),
            list: vec![Literal::String("Hovedrute".into()), Literal::String("Stor vej".into()),],
        }
    );
}

#[test]
fn not_in_list() {
    let e = parse_mapfile_expression("[v] NOT IN ('a', 'b')", 1).unwrap();
    assert_eq!(
        e,
        Expr::Not(Box::new(Expr::In {
            lhs: Box::new(Expr::Ident("v".into())),
            list: vec![Literal::String("a".into()), Literal::String("b".into())],
        }))
    );
}

#[test]
fn unsupported_operator_is_typed() {
    let err = parse_mapfile_expression("[a] =~ '/regex/'", 5).unwrap_err();
    assert_eq!(
        err,
        ExpressionError::Unsupported {
            op: "~".to_string(),
            line: 5,
        }
    );
}

#[test]
fn unsupported_function_call() {
    let err = parse_mapfile_expression("func(x)", 2).unwrap_err();
    assert_eq!(
        err,
        ExpressionError::Unsupported {
            op: "func".to_string(),
            line: 2,
        }
    );
}

#[test]
fn or_chain() {
    let e = parse_mapfile_expression("[a] = '1' OR [b] = '2' OR [c] = '3'", 1).unwrap();
    assert_eq!(
        e,
        Expr::Logic {
            op: LogicOp::Or,
            args: vec![
                Expr::Cmp {
                    op: CmpOp::Eq,
                    lhs: Box::new(Expr::Ident("a".into())),
                    rhs: Box::new(Expr::Literal(Literal::String("1".into()))),
                },
                Expr::Cmp {
                    op: CmpOp::Eq,
                    lhs: Box::new(Expr::Ident("b".into())),
                    rhs: Box::new(Expr::Literal(Literal::String("2".into()))),
                },
                Expr::Cmp {
                    op: CmpOp::Eq,
                    lhs: Box::new(Expr::Ident("c".into())),
                    rhs: Box::new(Expr::Literal(Literal::String("3".into()))),
                },
            ],
        }
    );
}

#[test]
fn parens_grouping() {
    let e = parse_mapfile_expression("([a] = '1' OR [b] = '2') AND [c] = '3'", 1).unwrap();
    assert_eq!(
        e,
        Expr::Logic {
            op: LogicOp::And,
            args: vec![
                Expr::Logic {
                    op: LogicOp::Or,
                    args: vec![
                        Expr::Cmp {
                            op: CmpOp::Eq,
                            lhs: Box::new(Expr::Ident("a".into())),
                            rhs: Box::new(Expr::Literal(Literal::String("1".into()))),
                        },
                        Expr::Cmp {
                            op: CmpOp::Eq,
                            lhs: Box::new(Expr::Ident("b".into())),
                            rhs: Box::new(Expr::Literal(Literal::String("2".into()))),
                        },
                    ],
                },
                Expr::Cmp {
                    op: CmpOp::Eq,
                    lhs: Box::new(Expr::Ident("c".into())),
                    rhs: Box::new(Expr::Literal(Literal::String("3".into()))),
                },
            ],
        }
    );
}

#[test]
fn number_literal() {
    let e = parse_mapfile_expression("[x] = 42", 1).unwrap();
    assert_eq!(
        e,
        Expr::Cmp {
            op: CmpOp::Eq,
            lhs: Box::new(Expr::Ident("x".into())),
            rhs: Box::new(Expr::Literal(Literal::Int(42))),
        }
    );
}

#[test]
fn float_literal() {
    let e = parse_mapfile_expression("[x] = 2.5", 1).unwrap();
    assert_eq!(
        e,
        Expr::Cmp {
            op: CmpOp::Eq,
            lhs: Box::new(Expr::Ident("x".into())),
            rhs: Box::new(Expr::Literal(Literal::Float(2.5))),
        }
    );
}

#[test]
fn quoted_string_inside_double_quotes() {
    // mapfile: EXPRESSION "[a] = 'hello'"
    let e = parse_mapfile_expression("[a] = 'hello'", 1).unwrap();
    assert_eq!(
        e,
        Expr::Cmp {
            op: CmpOp::Eq,
            lhs: Box::new(Expr::Ident("a".into())),
            rhs: Box::new(Expr::Literal(Literal::String("hello".into()))),
        }
    );
}

#[test]
fn empty_in_list() {
    let e = parse_mapfile_expression("[x] IN ()", 1).unwrap();
    assert!(matches!(
        e,
        Expr::In { list, .. } if list.is_empty()
    ));
}

// ----------------------------------------- new ops widening parity

fn cmp(op: CmpOp, lhs: &str, rhs: Expr) -> Expr {
    Expr::Cmp {
        op,
        lhs: Box::new(Expr::Ident(lhs.into())),
        rhs: Box::new(rhs),
    }
}

#[test]
fn numeric_cmp_ops() {
    let lit = |n: i64| Expr::Literal(Literal::Int(n));
    for (src, op) in [
        ("[a] < 5", CmpOp::Lt),
        ("[a] <= 5", CmpOp::Le),
        ("[a] > 5", CmpOp::Gt),
        ("[a] >= 5", CmpOp::Ge),
    ] {
        let e = parse_mapfile_expression(src, 1).unwrap();
        assert_eq!(e, cmp(op, "a", lit(5)), "for input {src}");
    }
}

#[test]
fn bang_eq_is_ne() {
    let e = parse_mapfile_expression("[a] != 5", 1).unwrap();
    assert_eq!(e, cmp(CmpOp::Ne, "a", Expr::Literal(Literal::Int(5))));
}

#[test]
fn keyword_cmp_aliases() {
    let int = |n: i64| Expr::Literal(Literal::Int(n));
    let s = |v: &str| Expr::Literal(Literal::String(v.into()));
    let cases = [
        ("[a] eq 'x'", CmpOp::Eq, s("x")),
        ("[a] ne 'x'", CmpOp::Ne, s("x")),
        ("[a] lt 5", CmpOp::Lt, int(5)),
        ("[a] le 5", CmpOp::Le, int(5)),
        ("[a] gt 5", CmpOp::Gt, int(5)),
        ("[a] ge 5", CmpOp::Ge, int(5)),
    ];
    for (src, op, rhs) in cases {
        let e = parse_mapfile_expression(src, 1).unwrap();
        assert_eq!(e, cmp(op, "a", rhs.clone()), "for input {src}");
    }
}

#[test]
fn c_style_logic_symbols() {
    let int = |n: i64| Expr::Literal(Literal::Int(n));
    let and = parse_mapfile_expression("[a] = 1 && [b] = 2", 1).unwrap();
    assert_eq!(
        and,
        Expr::Logic {
            op: LogicOp::And,
            args: vec![cmp(CmpOp::Eq, "a", int(1)), cmp(CmpOp::Eq, "b", int(2))],
        }
    );
    let or = parse_mapfile_expression("[a] = 1 || [b] = 2", 1).unwrap();
    assert_eq!(
        or,
        Expr::Logic {
            op: LogicOp::Or,
            args: vec![cmp(CmpOp::Eq, "a", int(1)), cmp(CmpOp::Eq, "b", int(2))],
        }
    );
    let bang = parse_mapfile_expression("!([a] = 1)", 1).unwrap();
    assert_eq!(bang, Expr::Not(Box::new(cmp(CmpOp::Eq, "a", int(1)))));
}

#[test]
fn like_pattern() {
    let e = parse_mapfile_expression("[a] LIKE 'foo%'", 1).unwrap();
    assert_eq!(
        e,
        Expr::Like {
            lhs: Box::new(Expr::Ident("a".into())),
            pattern: "foo%".to_string(),
        }
    );
}

#[test]
fn is_null_and_is_not_null() {
    let e = parse_mapfile_expression("[a] IS NULL", 1).unwrap();
    assert_eq!(e, Expr::IsNull(Box::new(Expr::Ident("a".into()))));
    let e = parse_mapfile_expression("[a] IS NOT NULL", 1).unwrap();
    assert_eq!(e, Expr::IsNotNull(Box::new(Expr::Ident("a".into()))));
}

#[test]
fn boolean_literals() {
    let e = parse_mapfile_expression("[active] = TRUE", 1).unwrap();
    assert_eq!(e, cmp(CmpOp::Eq, "active", Expr::Literal(Literal::Bool(true))));
    let e = parse_mapfile_expression("[deleted] = false", 1).unwrap();
    assert_eq!(e, cmp(CmpOp::Eq, "deleted", Expr::Literal(Literal::Bool(false))));
}

#[test]
fn naked_null_literal_rejected() {
    // mars_expr rejects `a = NULL`; mirror that so we never emit DSL
    // that fails to recompile downstream.
    let err = parse_mapfile_expression("[a] = NULL", 1).unwrap_err();
    assert!(matches!(err, ExpressionError::Parse { .. }), "got {err:?}");
}

#[test]
fn no_cmp_chaining() {
    // `a = b = c` would form invalid DSL on round-trip; reject early.
    let err = parse_mapfile_expression("[a] = 1 = 2", 1).unwrap_err();
    assert!(matches!(err, ExpressionError::Parse { .. }), "got {err:?}");
}

#[test]
fn set_literal_numeric() {
    let lits = parse_set_literal("{14, 15, 984}", 1).unwrap();
    assert_eq!(lits, vec![Literal::Int(14), Literal::Int(15), Literal::Int(984)]);
}

#[test]
fn set_literal_single() {
    let lits = parse_set_literal("{14}", 1).unwrap();
    assert_eq!(lits, vec![Literal::Int(14)]);
}

#[test]
fn set_literal_barewords_are_strings() {
    // barewords inside a set are mapfile shorthand for string values.
    let lits = parse_set_literal("{skovPlantage,agerMark,eng}", 1).unwrap();
    assert_eq!(
        lits,
        vec![
            Literal::String("skovPlantage".into()),
            Literal::String("agerMark".into()),
            Literal::String("eng".into()),
        ]
    );
}

#[test]
fn set_literal_quoted_strings() {
    let lits = parse_set_literal("{'a','b'}", 1).unwrap();
    assert_eq!(lits, vec![Literal::String("a".into()), Literal::String("b".into())]);
}

#[test]
fn set_literal_empty() {
    let lits = parse_set_literal("{}", 1).unwrap();
    assert!(lits.is_empty());
}

#[test]
fn set_literal_unterminated_is_parse_error() {
    let err = parse_set_literal("{14", 1).unwrap_err();
    assert!(matches!(err, ExpressionError::Parse { .. }), "got {err:?}");
}

#[test]
fn set_literal_unopened_is_parse_error() {
    let err = parse_set_literal("14}", 1).unwrap_err();
    assert!(matches!(err, ExpressionError::Parse { .. }), "got {err:?}");
}

#[test]
fn roundtrip_through_mars_expr_parse() {
    // every importer-emitted expression must reparse cleanly through the
    // mars_expr DSL parser, since that is what the YAML pipeline consumes.
    let inputs = [
        "[a] < 5",
        "[a] >= 5",
        "[a] != 5",
        "[a] eq 'x'",
        "[a] gt 5",
        "[a] LIKE 'foo%'",
        "[a] IS NULL",
        "[a] IS NOT NULL",
        "[a] = TRUE",
        "[a] = 1 && [b] = 2",
        "[a] = 1 || [b] = 2",
        "!([a] = 1)",
        "[a] IN (1, 2, 3)",
        "[a] NOT IN ('x', 'y')",
    ];
    for src in inputs {
        let e = parse_mapfile_expression(src, 1).unwrap_or_else(|err| panic!("parse `{src}`: {err}"));
        let emitted = format!("{e}");
        let reparsed = mars_expr::parse(&emitted)
            .unwrap_or_else(|err| panic!("mars_expr can't reparse `{emitted}` (from `{src}`): {err}"));
        assert_eq!(e, reparsed, "ast drift `{src}` -> `{emitted}`");
    }
}
