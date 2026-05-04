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

/// Parser entry point. The grammar is small enough that a hand-rolled parser
/// will live here in Phase 1; for now this returns `NotImplemented` so callers
/// can wire the API end-to-end without committing to a parser implementation.
pub fn parse(input: &str) -> Result<Expr, ExprError> {
    if input.is_empty() {
        return Err(ExprError::Parse("empty expression".into()));
    }
    Err(ExprError::NotImplemented {
        what: "mars-expr::parse",
    })
}

/// In-memory evaluator. `attrs` is the row's attribute map.
pub fn eval(_expr: &Expr, _attrs: &dyn AttributeAccess) -> Result<Literal, ExprError> {
    Err(ExprError::NotImplemented {
        what: "mars-expr::eval",
    })
}

/// Attribute access for the in-memory evaluator. The runtime feeds this from
/// the artifact's columnar attribute block.
pub trait AttributeAccess {
    fn get(&self, name: &str) -> Option<Literal>;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_errors() {
        assert!(matches!(parse(""), Err(ExprError::Parse(_))));
    }

    #[test]
    fn parse_returns_not_implemented_for_real_input() {
        // ensures the api surface matches what the rest of the workspace expects
        assert!(matches!(parse("a = 1"), Err(ExprError::NotImplemented { .. })));
    }
}
