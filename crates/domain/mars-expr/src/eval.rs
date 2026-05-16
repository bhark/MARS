//! sql three-valued-logic evaluator over `&dyn AttributeAccess`.

use crate::{AttributeAccess, CmpOp, Expr, ExprError, Literal, LogicOp};

pub(crate) fn eval(expr: &Expr, attrs: &dyn AttributeAccess) -> Result<Literal, ExprError> {
    match expr {
        Expr::Literal(l) => Ok(l.clone()),
        Expr::Ident(name) => attrs.get(name).ok_or_else(|| ExprError::UnknownIdent(name.clone())),
        Expr::Cmp { op, lhs, rhs } => {
            let l = eval(lhs, attrs)?;
            let r = eval(rhs, attrs)?;
            cmp(*op, &l, &r)
        }
        Expr::Logic { op, args } => match op {
            LogicOp::And => {
                let mut any_null = false;
                for a in args {
                    match eval(a, attrs)? {
                        Literal::Bool(false) => return Ok(Literal::Bool(false)),
                        Literal::Bool(true) => {}
                        Literal::Null => any_null = true,
                        other => {
                            return Err(ExprError::Type(format!("AND requires boolean operands, got {other:?}")));
                        }
                    }
                }
                Ok(if any_null { Literal::Null } else { Literal::Bool(true) })
            }
            LogicOp::Or => {
                let mut any_null = false;
                for a in args {
                    match eval(a, attrs)? {
                        Literal::Bool(true) => return Ok(Literal::Bool(true)),
                        Literal::Bool(false) => {}
                        Literal::Null => any_null = true,
                        other => {
                            return Err(ExprError::Type(format!("OR requires boolean operands, got {other:?}")));
                        }
                    }
                }
                Ok(if any_null { Literal::Null } else { Literal::Bool(false) })
            }
        },
        Expr::Not(inner) => match eval(inner, attrs)? {
            Literal::Bool(b) => Ok(Literal::Bool(!b)),
            Literal::Null => Ok(Literal::Null),
            other => Err(ExprError::Type(format!("NOT requires boolean operand, got {other:?}"))),
        },
        Expr::In { lhs, list } => {
            let v = eval(lhs, attrs)?;
            if matches!(v, Literal::Null) {
                return Ok(Literal::Null);
            }
            if list.is_empty() {
                // sql: `x IN ()` is false
                return Ok(Literal::Bool(false));
            }
            let mut any_null = false;
            for cand in list {
                if matches!(cand, Literal::Null) {
                    any_null = true;
                    continue;
                }
                match cmp(CmpOp::Eq, &v, cand)? {
                    Literal::Bool(true) => return Ok(Literal::Bool(true)),
                    Literal::Null => any_null = true,
                    _ => {}
                }
            }
            Ok(if any_null { Literal::Null } else { Literal::Bool(false) })
        }
        Expr::Like { lhs, pattern } => {
            let v = eval(lhs, attrs)?;
            match v {
                Literal::Null => Ok(Literal::Null),
                Literal::String(s) => Ok(Literal::Bool(like_match(&s, pattern))),
                other => Err(ExprError::Type(format!("LIKE requires string operand, got {other:?}"))),
            }
        }
        Expr::Regex {
            lhs,
            pattern,
            case_insensitive,
        } => {
            let v = eval(lhs, attrs)?;
            match v {
                Literal::Null => Ok(Literal::Null),
                Literal::String(s) => {
                    let re = compile_regex(pattern, *case_insensitive)?;
                    Ok(Literal::Bool(re.is_match(&s)))
                }
                other => Err(ExprError::Type(format!("~ requires string operand, got {other:?}"))),
            }
        }
        Expr::IsNull(inner) => Ok(Literal::Bool(matches!(eval(inner, attrs)?, Literal::Null))),
        Expr::IsNotNull(inner) => Ok(Literal::Bool(!matches!(eval(inner, attrs)?, Literal::Null))),
    }
}

fn compile_regex(pattern: &str, case_insensitive: bool) -> Result<regex::Regex, ExprError> {
    regex::RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|e| ExprError::InvalidRegex {
            pattern: pattern.to_string(),
            msg: e.to_string(),
        })
}

/// Three-valued-logic comparator used by both `Expr::Cmp` and `Expr::In`.
///
/// Mixed Int/Float comparisons cast the int to f64. For ints whose magnitude
/// exceeds 2^53 this loses precision: e.g. `i64::MAX` and `i64::MAX - 1` both
/// promote to the same f64 and compare equal. The dialect does not pin mixed-
/// type comparison semantics, so the lossy cast is accepted; callers needing
/// exact comparison should keep both sides Int.
fn cmp(op: CmpOp, l: &Literal, r: &Literal) -> Result<Literal, ExprError> {
    if matches!(l, Literal::Null) || matches!(r, Literal::Null) {
        return Ok(Literal::Null);
    }
    let ord = match (l, r) {
        (Literal::Int(a), Literal::Int(b)) => a.cmp(b),
        (Literal::Float(a), Literal::Float(b)) => a
            .partial_cmp(b)
            .ok_or_else(|| ExprError::Type("NaN comparison".into()))?,
        (Literal::Int(a), Literal::Float(b)) => (*a as f64)
            .partial_cmp(b)
            .ok_or_else(|| ExprError::Type("NaN comparison".into()))?,
        (Literal::Float(a), Literal::Int(b)) => a
            .partial_cmp(&(*b as f64))
            .ok_or_else(|| ExprError::Type("NaN comparison".into()))?,
        (Literal::String(a), Literal::String(b)) => a.cmp(b),
        (Literal::Bool(a), Literal::Bool(b)) => a.cmp(b),
        (a, b) => {
            return Err(ExprError::Type(format!("cannot compare {a:?} and {b:?}")));
        }
    };
    use std::cmp::Ordering::*;
    let res = match (op, ord) {
        (CmpOp::Eq, Equal) => true,
        (CmpOp::Eq, _) => false,
        (CmpOp::Ne, Equal) => false,
        (CmpOp::Ne, _) => true,
        (CmpOp::Lt, Less) => true,
        (CmpOp::Lt, _) => false,
        (CmpOp::Le, Greater) => false,
        (CmpOp::Le, _) => true,
        (CmpOp::Gt, Greater) => true,
        (CmpOp::Gt, _) => false,
        (CmpOp::Ge, Less) => false,
        (CmpOp::Ge, _) => true,
    };
    Ok(Literal::Bool(res))
}

fn like_match(s: &str, pattern: &str) -> bool {
    // greedy linear scan: '%' matches any sequence, '_' matches one char.
    // O(n + m) time, O(1) space. behaves like sql LIKE (no escape char).
    let s: Vec<char> = s.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let mut si = 0;
    let mut pi = 0;
    let mut star_idx: Option<usize> = None;
    let mut match_idx = 0;

    while si < s.len() {
        if pi < p.len() && (p[pi] == '_' || p[pi] == s[si]) {
            si += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '%' {
            star_idx = Some(pi);
            match_idx = si;
            pi += 1;
        } else if let Some(star) = star_idx {
            pi = star + 1;
            match_idx += 1;
            si = match_idx;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == '%' {
        pi += 1;
    }

    pi == p.len()
}
