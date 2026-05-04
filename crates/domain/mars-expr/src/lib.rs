//! MARS embedded expression language. Used in `when:` filters and `text:`
//! interpolations. Maps 1:1 to PostgreSQL `WHERE` semantics so the same AST
//! can be lowered into a parameterised SQL query (in `mars-source-postgres`)
//! and evaluated in-memory at render time.
//!
//! SPEC §5.6 defines the dialect. This crate owns the AST, parser, validator,
//! and in-memory evaluator. SQL lowering lives with the database adapter that
//! owns the database vocabulary; that boundary keeps SQL parameterisation
//! enforceable.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::fmt;

mod eval;
mod parser;

#[derive(Debug, thiserror::Error)]
pub enum ExprError {
    #[error("parse error: {0}")]
    Parse(String),
    #[error("type error: {0}")]
    Type(String),
    #[error("unknown identifier: {0}")]
    UnknownIdent(String),
    #[error("not implemented: {what}")]
    NotImplemented { what: &'static str },
}

/// Filter-expression AST. Scope is intentionally narrow (SPEC §5.6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    Literal(Literal),
    Ident(String),
    Cmp { op: CmpOp, lhs: Box<Expr>, rhs: Box<Expr> },
    Logic { op: LogicOp, args: Vec<Expr> },
    Not(Box<Expr>),
    In { lhs: Box<Expr>, list: Vec<Literal> },
    Like { lhs: Box<Expr>, pattern: String },
    IsNull(Box<Expr>),
    IsNotNull(Box<Expr>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Literal {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogicOp {
    And,
    Or,
}

/// Parser entry point.
pub fn parse(input: &str) -> Result<Expr, ExprError> {
    parser::parse(input)
}

/// In-memory evaluator. `attrs` is the row's attribute map.
pub fn eval(expr: &Expr, attrs: &dyn AttributeAccess) -> Result<Literal, ExprError> {
    eval::eval(expr, attrs)
}

/// Attribute access for the in-memory evaluator. The runtime feeds this from
/// the artifact's columnar attribute block.
pub trait AttributeAccess {
    fn get(&self, name: &str) -> Option<Literal>;
}

// `Display` re-emits valid grammar so a parsed `Expr` can round-trip through
// `parse(format!("{e}"))`. Comparison precedence is below logic-not, so cmp
// and friends do not need parens; logic ops do.

impl fmt::Display for CmpOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CmpOp::Eq => "=",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        })
    }
}

impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Literal::Null => f.write_str("NULL"),
            Literal::Bool(true) => f.write_str("TRUE"),
            Literal::Bool(false) => f.write_str("FALSE"),
            Literal::Int(n) => write!(f, "{n}"),
            Literal::Float(v) => {
                // ensure float renders with a dot so the lexer reads it as Float
                if v.is_finite() && v.fract() == 0.0 {
                    write!(f, "{v:.1}")
                } else {
                    write!(f, "{v}")
                }
            }
            Literal::String(s) => write_quoted(f, s),
        }
    }
}

fn write_quoted(f: &mut fmt::Formatter<'_>, s: &str) -> fmt::Result {
    f.write_str("'")?;
    for c in s.chars() {
        if c == '\'' {
            f.write_str("''")?;
        } else {
            f.write_str(&c.to_string())?;
        }
    }
    f.write_str("'")
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Literal(l) => write!(f, "{l}"),
            Expr::Ident(name) => f.write_str(name),
            Expr::Cmp { op, lhs, rhs } => write!(f, "{lhs} {op} {rhs}"),
            Expr::Logic { op, args } => {
                let sep = match op {
                    LogicOp::And => " AND ",
                    LogicOp::Or => " OR ",
                };
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(sep)?;
                    }
                    write_logic_arg(f, op, a)?;
                }
                Ok(())
            }
            Expr::Not(inner) => {
                f.write_str("NOT ")?;
                write_not_arg(f, inner)
            }
            Expr::In { lhs, list } => {
                write!(f, "{lhs} IN (")?;
                for (i, lit) in list.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{lit}")?;
                }
                f.write_str(")")
            }
            Expr::Like { lhs, pattern } => {
                write!(f, "{lhs} LIKE ")?;
                write_quoted(f, pattern)
            }
            Expr::IsNull(inner) => write!(f, "{inner} IS NULL"),
            Expr::IsNotNull(inner) => write!(f, "{inner} IS NOT NULL"),
        }
    }
}

fn write_logic_arg(f: &mut fmt::Formatter<'_>, parent: &LogicOp, e: &Expr) -> fmt::Result {
    // parenthesise children of lower or equal precedence to preserve grouping.
    // OR is lowest, AND is above OR. NOT binds tighter than both, so NOT never
    // needs parens here. Same op needs no parens (left-assoc / flattened).
    let needs_paren = matches!((parent, e), (LogicOp::And, Expr::Logic { op: LogicOp::Or, .. }));
    if needs_paren {
        write!(f, "({e})")
    } else {
        write!(f, "{e}")
    }
}

fn write_not_arg(f: &mut fmt::Formatter<'_>, e: &Expr) -> fmt::Result {
    // NOT binds tighter than AND/OR, so wrap any logic expr.
    if matches!(e, Expr::Logic { .. }) {
        write!(f, "({e})")
    } else {
        write!(f, "{e}")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
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
    fn reject_regex() {
        assert!(matches!(parse("a ~ '/regex/'"), Err(ExprError::Parse(_))));
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
    fn eval_three_valued_eq_null() {
        let e = parse("a = NULL").unwrap();
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
    fn eval_int_float_promotion() {
        let e = parse("x >= 10").unwrap();
        assert_eq!(
            eval(&e, &attrs(&[("x", Literal::Float(10.5))])).unwrap(),
            Literal::Bool(true)
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod proptests {
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
            (arb_cmp_op(), arb_primary(), arb_primary()).prop_map(|(op, l, r)| Expr::Cmp {
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
}
