//! MARS embedded expression language. Used in `when:` filters and `text:`
//! interpolations. Maps 1:1 to PostgreSQL `WHERE` semantics so the same AST
//! can be lowered into a parameterised SQL query (in `mars-source-postgres`)
//! and evaluated in-memory at render time.
//!
//! This crate owns the AST, parser, validator,
//! and in-memory evaluator. SQL lowering lives with the database adapter that
//! owns the database vocabulary; that boundary keeps SQL parameterisation
//! enforceable.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::fmt;

mod eval;
pub mod interpolate;
mod parser;

pub use interpolate::{Segment, Template, eval_template, parse_template};

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
    #[error("expression nesting too deep (max {max})")]
    TooDeep { max: u32 },
    #[error("invalid regex `{pattern}`: {msg}")]
    InvalidRegex { pattern: String, msg: String },
}

/// Filter-expression AST. Scope is intentionally narrow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    Literal(Literal),
    Ident(String),
    Cmp {
        op: CmpOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Logic {
        op: LogicOp,
        args: Vec<Expr>,
    },
    Not(Box<Expr>),
    In {
        lhs: Box<Expr>,
        list: Vec<Literal>,
    },
    Like {
        lhs: Box<Expr>,
        pattern: String,
    },
    // postgres-style regex match. `~` is case-sensitive, `~*` case-insensitive.
    // pattern flavor follows the `regex` crate (rust regex, similar to RE2);
    // it is not a verbatim posix superset of postgres `~`, but covers the
    // mapfile CLASSITEM `/pat/` form we lift from importers.
    Regex {
        lhs: Box<Expr>,
        pattern: String,
        case_insensitive: bool,
    },
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

/// Zero-sized attribute access that returns `None` for every column. Useful
/// at seams that resolve sizes / angles without a feature in scope (legends,
/// error overlays, tests).
#[derive(Debug, Clone, Copy, Default)]
pub struct NullAttributes;

impl AttributeAccess for NullAttributes {
    fn get(&self, _: &str) -> Option<Literal> {
        None
    }
}

/// Collect every identifier name referenced by `expr` into `out`. Used by
/// config validation to confirm each binding materialises the attributes its
/// layers consume.
pub fn collect_idents(expr: &Expr, out: &mut std::collections::BTreeSet<String>) {
    match expr {
        Expr::Literal(_) => {}
        Expr::Ident(name) => {
            out.insert(name.clone());
        }
        Expr::Cmp { lhs, rhs, .. } => {
            collect_idents(lhs, out);
            collect_idents(rhs, out);
        }
        Expr::Logic { args, .. } => {
            for a in args {
                collect_idents(a, out);
            }
        }
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => collect_idents(inner, out),
        Expr::In { lhs, .. } | Expr::Like { lhs, .. } | Expr::Regex { lhs, .. } => collect_idents(lhs, out),
    }
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
                // must always emit '.' or 'e' so the lexer reparses as Float, not Int.
                // bare `{v}` for 1e20 yields "100000000000000000000" which round-trips
                // as Int and silently loses the Float type.
                if !v.is_finite() {
                    return Err(fmt::Error);
                }
                if v.fract() == 0.0 {
                    write!(f, "{v:.1}")
                } else {
                    let s = format!("{v}");
                    if s.contains('.') || s.contains('e') || s.contains('E') {
                        f.write_str(&s)
                    } else {
                        write!(f, "{s}.0")
                    }
                }
            }
            Literal::String(s) => write_quoted(f, s),
        }
    }
}

fn write_quoted(f: &mut fmt::Formatter<'_>, s: &str) -> fmt::Result {
    use std::fmt::Write as _;
    f.write_str("'")?;
    for c in s.chars() {
        if c == '\'' {
            f.write_str("''")?;
        } else {
            f.write_char(c)?;
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
            Expr::Regex {
                lhs,
                pattern,
                case_insensitive,
            } => {
                let op = if *case_insensitive { "~*" } else { "~" };
                write!(f, "{lhs} {op} ")?;
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
mod tests;

#[cfg(test)]
mod proptests;
