//! tokenizer + recursive-descent parser for the dialect.
//!
//! grammar (low → high precedence):
//!   or      := and ( OR and )*
//!   and     := not ( AND not )*
//!   not     := NOT not | predicate
//!   predicate := primary postfix?
//!   postfix := IS [NOT] NULL
//!            | [NOT] IN '(' literal_list ')'
//!            | [NOT] LIKE string
//!            | ('~' | '~*') string
//!            | cmp_op primary
//!   primary := literal | ident | '(' or ')'

use crate::{Expr, ExprError};

mod grammar;
mod lexer;

use self::grammar::Parser;
use self::lexer::tokenize;

/// Maximum recursive depth across `parse_or` / `parse_and` / `parse_not` /
/// `parse_primary`. Bounds stack growth on adversarial input like `NOT NOT...`
/// or `(((...)))`. 64 levels is well above any sane filter expression.
pub(super) const MAX_DEPTH: u32 = 64;

pub(super) fn parse_err(pos: usize, msg: &str) -> ExprError {
    ExprError::Parse(format!("at position {pos}: {msg}"))
}

pub(crate) fn parse(input: &str) -> Result<Expr, ExprError> {
    if input.trim().is_empty() {
        return Err(ExprError::Parse("empty expression".into()));
    }
    let toks = tokenize(input)?;
    let end_pos = input.len();
    let mut p = Parser::new(toks, end_pos);
    let expr = p.parse_or()?;
    if !p.at_end() {
        return Err(parse_err(p.pos(), "trailing tokens after expression"));
    }
    Ok(expr)
}
