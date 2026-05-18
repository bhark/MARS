//! mapfile-flavoured EXPRESSION parser, lowering to `mars_expr::Expr`.
//!
//! supported subset:
//! - logic: AND/OR/NOT (also && / || / !)
//! - cmp: `=`, `!=`, `<>`, `<`, `<=`, `>`, `>=` and the MapServer keyword
//!   aliases eq / ne / lt / le / gt / ge
//! - postfix predicates: IN (..), NOT IN (..), LIKE 'pat', IS [NOT] NULL
//! - operands: bracketed attribute refs `[name]`, numeric, string, TRUE/FALSE
//!
//! anything outside this set returns `ExpressionError` so the caller can emit
//! a # TODO: hand-translate comment + warning. notable still-unsupported:
//! regex (`=~`, `~`, `~*`), arithmetic, function calls.

mod lexer;
mod parser;

use mars_expr::{Expr, Literal};

use self::lexer::Lexer;
use self::parser::Parser;

#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub(crate) enum ExpressionError {
    #[error("unsupported expression operator `{op}` at line {line}")]
    Unsupported { op: String, line: usize },
    #[error("expression parse error at line {line}: {msg}")]
    Parse { msg: String, line: usize },
}

/// parse a mapfile expression string into a `mars_expr::Expr`.
pub(crate) fn parse_mapfile_expression(input: &str, line: usize) -> Result<Expr, ExpressionError> {
    let mut lexer = Lexer::new(input, line);
    let tokens = lexer.run()?;
    let mut parser = Parser::new(&tokens, line);
    let expr = parser.parse_expr()?;
    parser.expect_eof()?;
    Ok(expr)
}

/// parse a mapfile `EXPRESSION { v1, v2, ... }` set form into a flat list of
/// literals. caller (resolve_class) wraps these into a CLASSITEM-qualified
/// `IN (...)` predicate. empty `{}` returns an empty vec.
///
/// values may be numeric, single- or double-quoted strings, or unquoted
/// barewords. barewords lower to `Literal::String` since they have no
/// column-reference role inside set braces.
pub(crate) fn parse_set_literal(input: &str, line: usize) -> Result<Vec<Literal>, ExpressionError> {
    let trimmed = input.trim();
    let body = trimmed
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .ok_or_else(|| ExpressionError::Parse {
            msg: "set form must be enclosed in `{...}`".to_string(),
            line,
        })?
        .trim();
    if body.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for ch in body.chars() {
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
                buf.push(ch);
            }
            '"' if !in_single => {
                in_double = !in_double;
                buf.push(ch);
            }
            ',' if !in_single && !in_double => {
                out.push(classify_set_item(buf.trim(), line)?);
                buf.clear();
            }
            _ => buf.push(ch),
        }
    }
    if in_single || in_double {
        return Err(ExpressionError::Parse {
            msg: "unterminated string literal in set form".to_string(),
            line,
        });
    }
    out.push(classify_set_item(buf.trim(), line)?);
    Ok(out)
}

fn classify_set_item(s: &str, line: usize) -> Result<Literal, ExpressionError> {
    if s.is_empty() {
        return Err(ExpressionError::Parse {
            msg: "empty value in set form".to_string(),
            line,
        });
    }
    if let Some(rest) = s.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')) {
        return Ok(Literal::String(rest.to_string()));
    }
    if let Some(rest) = s.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        return Ok(Literal::String(rest.to_string()));
    }
    Ok(number_or_string(s))
}

// mapfile values like `12-` / `2.5-12` / `0-2.5` are bareword string literals,
// not numbers. fall back to a string when the token doesn't parse cleanly.
pub(crate) fn number_or_string(s: &str) -> Literal {
    if let Ok(n) = s.parse::<i64>() {
        Literal::Int(n)
    } else if let Ok(f) = s.parse::<f64>()
        && f.is_finite()
    {
        Literal::Float(f)
    } else {
        Literal::String(s.to_string())
    }
}

#[cfg(test)]
mod tests;
