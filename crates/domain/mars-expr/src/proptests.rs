#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use proptest::prelude::*;

fn arb_ident() -> impl Strategy<Value = String> {
    // start with letter, follow with letters/digits/underscore; avoid keywords
    "[a-z][a-z0-9_]{0,7}".prop_filter("not a keyword", |s| {
        !matches!(
            s.to_ascii_uppercase().as_str(),
            "AND" | "OR" | "NOT" | "IN" | "LIKE" | "IS" | "NULL" | "TRUE" | "FALSE"
        )
    })
}

fn arb_literal() -> impl Strategy<Value = Literal> {
    prop_oneof![
        Just(Literal::Null),
        any::<bool>().prop_map(Literal::Bool),
        any::<i32>().prop_map(|n| Literal::Int(n.into())),
        // restrict to printable ascii without single quote backslash drama; doubled quotes handled.
        "[a-zA-Z0-9 ]{0,8}".prop_map(Literal::String),
    ]
}

fn arb_cmp_op() -> impl Strategy<Value = CmpOp> {
    prop_oneof![
        Just(CmpOp::Eq),
        Just(CmpOp::Ne),
        Just(CmpOp::Lt),
        Just(CmpOp::Le),
        Just(CmpOp::Gt),
        Just(CmpOp::Ge),
    ]
}

// primary = literal or ident. cmp / in / like / is-null lhs/rhs must be a
// primary because the parser's postfix predicates don't chain on each other.
fn arb_primary() -> impl Strategy<Value = Expr> {
    prop_oneof![arb_literal().prop_map(Expr::Literal), arb_ident().prop_map(Expr::Ident),]
}

fn arb_predicate() -> impl Strategy<Value = Expr> {
    prop_oneof![
        arb_primary(),
        (arb_cmp_op(), arb_primary(), arb_primary())
            // parser rejects = NULL / != NULL; filter so generated ASTs
            // round-trip through Display->parse without tripping that.
            .prop_filter("eq/ne with NULL operand is rejected by parser", |(op, l, r)| {
                if !matches!(op, CmpOp::Eq | CmpOp::Ne) {
                    return true;
                }
                !matches!(l, Expr::Literal(Literal::Null)) && !matches!(r, Expr::Literal(Literal::Null))
            })
            .prop_map(|(op, l, r)| Expr::Cmp {
                op,
                lhs: Box::new(l),
                rhs: Box::new(r),
            }),
        (arb_primary(), prop::collection::vec(arb_literal(), 0..4)).prop_map(|(lhs, list)| Expr::In {
            lhs: Box::new(lhs),
            list
        }),
        (arb_primary(), "[a-zA-Z0-9_%]{0,8}").prop_map(|(lhs, pat)| Expr::Like {
            lhs: Box::new(lhs),
            pattern: pat
        }),
        // restrict to alnum patterns so the generated regex stays
        // parser-printable (no embedded single quotes) and valid; depth of
        // escape semantics is covered by the unit tests above.
        (arb_primary(), "[a-zA-Z0-9_]{0,8}", any::<bool>()).prop_map(|(lhs, pat, ci)| Expr::Regex {
            lhs: Box::new(lhs),
            pattern: pat,
            case_insensitive: ci,
        }),
        arb_primary().prop_map(|e| Expr::IsNull(Box::new(e))),
        arb_primary().prop_map(|e| Expr::IsNotNull(Box::new(e))),
    ]
}

fn arb_expr() -> impl Strategy<Value = Expr> {
    arb_predicate().prop_recursive(4, 24, 4, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone()).prop_map(|(a, b)| Expr::Logic {
                op: LogicOp::And,
                args: vec![a, b],
            }),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| Expr::Logic {
                op: LogicOp::Or,
                args: vec![a, b],
            }),
            inner.prop_map(|e| Expr::Not(Box::new(e))),
        ]
    })
}

// canonical form: same-op logic chains are flattened. parser does this on
// construction, so for round-trip equality we flatten the generated AST too.
fn canonicalize(e: Expr) -> Expr {
    match e {
        Expr::Logic { op, args } => {
            let mut flat = Vec::with_capacity(args.len());
            for a in args {
                let a = canonicalize(a);
                match a {
                    Expr::Logic {
                        op: inner_op,
                        args: inner_args,
                    } if inner_op == op => {
                        flat.extend(inner_args);
                    }
                    other => flat.push(other),
                }
            }
            Expr::Logic { op, args: flat }
        }
        Expr::Cmp { op, lhs, rhs } => Expr::Cmp {
            op,
            lhs: Box::new(canonicalize(*lhs)),
            rhs: Box::new(canonicalize(*rhs)),
        },
        Expr::Not(inner) => Expr::Not(Box::new(canonicalize(*inner))),
        Expr::In { lhs, list } => Expr::In {
            lhs: Box::new(canonicalize(*lhs)),
            list,
        },
        Expr::Like { lhs, pattern } => Expr::Like {
            lhs: Box::new(canonicalize(*lhs)),
            pattern,
        },
        Expr::Regex {
            lhs,
            pattern,
            case_insensitive,
        } => Expr::Regex {
            lhs: Box::new(canonicalize(*lhs)),
            pattern,
            case_insensitive,
        },
        Expr::IsNull(i) => Expr::IsNull(Box::new(canonicalize(*i))),
        Expr::IsNotNull(i) => Expr::IsNotNull(Box::new(canonicalize(*i))),
        other => other,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn display_roundtrips_through_parse(e in arb_expr()) {
        let canon = canonicalize(e);
        let s = format!("{canon}");
        let parsed = parse(&s).unwrap_or_else(|err| panic!("parse `{s}` failed: {err}"));
        prop_assert_eq!(canon, parsed);
    }
}
