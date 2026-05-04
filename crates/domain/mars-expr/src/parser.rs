//! tokenizer + recursive-descent parser for the SPEC §5.6 dialect.
//!
//! grammar (low → high precedence):
//!   or      := and ( OR and )*
//!   and     := not ( AND not )*
//!   not     := NOT not | predicate
//!   predicate := primary postfix?
//!   postfix := IS [NOT] NULL
//!            | [NOT] IN '(' literal_list ')'
//!            | [NOT] LIKE string
//!            | cmp_op primary
//!   primary := literal | ident | '(' or ')'

use crate::{CmpOp, Expr, ExprError, Literal, LogicOp};

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    LParen,
    RParen,
    Comma,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Ident(String),
    KwAnd,
    KwOr,
    KwNot,
    KwIn,
    KwLike,
    KwIs,
    KwNull,
    KwTrue,
    KwFalse,
    Str(String),
    Int(i64),
    Float(f64),
}

#[derive(Debug, Clone)]
struct Spanned {
    tok: Tok,
    pos: usize,
}

fn tokenize(input: &str) -> Result<Vec<Spanned>, ExprError> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let pos = i;
        match b {
            b'(' => {
                out.push(Spanned { tok: Tok::LParen, pos });
                i += 1;
            }
            b')' => {
                out.push(Spanned { tok: Tok::RParen, pos });
                i += 1;
            }
            b',' => {
                out.push(Spanned { tok: Tok::Comma, pos });
                i += 1;
            }
            b'=' => {
                out.push(Spanned { tok: Tok::Eq, pos });
                i += 1;
            }
            b'!' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    out.push(Spanned { tok: Tok::Ne, pos });
                    i += 2;
                } else {
                    return Err(parse_err(pos, "expected '!='"));
                }
            }
            b'<' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    out.push(Spanned { tok: Tok::Le, pos });
                    i += 2;
                } else if i + 1 < bytes.len() && bytes[i + 1] == b'>' {
                    out.push(Spanned { tok: Tok::Ne, pos });
                    i += 2;
                } else {
                    out.push(Spanned { tok: Tok::Lt, pos });
                    i += 1;
                }
            }
            b'>' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    out.push(Spanned { tok: Tok::Ge, pos });
                    i += 2;
                } else {
                    out.push(Spanned { tok: Tok::Gt, pos });
                    i += 1;
                }
            }
            b'\'' => {
                let (s, end) = read_string(bytes, i)?;
                out.push(Spanned { tok: Tok::Str(s), pos });
                i = end;
            }
            b'"' => return Err(parse_err(pos, "double-quoted strings are not allowed")),
            b'+' | b'*' | b'/' | b'~' | b'%' | b'^' | b'&' | b'|' | b'?' | b'@' | b'#' | b'$' | b':' | b';' => {
                return Err(parse_err(pos, &format!("unexpected character '{}'", b as char)));
            }
            b'-' => {
                // unary minus only on a following digit forms a negative numeric literal.
                // anything else (binary subtract / arithmetic) is rejected.
                if i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
                    let prev_allows_unary = match out.last().map(|s| &s.tok) {
                        None => true,
                        Some(t) => matches!(
                            t,
                            Tok::LParen
                                | Tok::Comma
                                | Tok::Eq
                                | Tok::Ne
                                | Tok::Lt
                                | Tok::Le
                                | Tok::Gt
                                | Tok::Ge
                                | Tok::KwAnd
                                | Tok::KwOr
                                | Tok::KwNot
                                | Tok::KwIn
                                | Tok::KwLike
                                | Tok::KwIs
                        ),
                    };
                    if !prev_allows_unary {
                        return Err(parse_err(pos, "arithmetic operators are not allowed"));
                    }
                    let (tok, end) = read_number(bytes, i + 1)?;
                    let neg = match tok {
                        Tok::Int(n) => Tok::Int(-n),
                        Tok::Float(f) => Tok::Float(-f),
                        other => other,
                    };
                    out.push(Spanned { tok: neg, pos });
                    i = end;
                } else {
                    return Err(parse_err(pos, "arithmetic operators are not allowed"));
                }
            }
            _ if b.is_ascii_digit() => {
                let (tok, end) = read_number(bytes, i)?;
                out.push(Spanned { tok, pos });
                i = end;
            }
            _ if b.is_ascii_alphabetic() || b == b'_' => {
                let (tok, end) = read_ident(bytes, i);
                out.push(Spanned { tok, pos });
                i = end;
            }
            _ => return Err(parse_err(pos, &format!("unexpected character '{}'", b as char))),
        }
    }
    Ok(out)
}

fn parse_err(pos: usize, msg: &str) -> ExprError {
    ExprError::Parse(format!("at position {pos}: {msg}"))
}

fn read_string(bytes: &[u8], start: usize) -> Result<(String, usize), ExprError> {
    // walk by char_indices on the post-quote slice so multi-byte utf-8 (e.g.
    // danish 'ø' in 'Foreløbig') is preserved instead of being byte-as-char'd.
    let body_start = start + 1;
    let s = std::str::from_utf8(&bytes[body_start..]).map_err(|_| parse_err(start, "invalid utf-8 in string literal"))?;
    let mut out = String::new();
    let mut iter = s.char_indices().peekable();
    while let Some((rel, c)) = iter.next() {
        if c == '\'' {
            if let Some(&(_, '\'')) = iter.peek() {
                out.push('\'');
                iter.next();
                continue;
            }
            // end_pos = absolute byte index just past the closing quote
            let end = body_start + rel + 1;
            return Ok((out, end));
        }
        out.push(c);
    }
    Err(parse_err(start, "unterminated string literal"))
}

fn read_number(bytes: &[u8], start: usize) -> Result<(Tok, usize), ExprError> {
    let mut i = start;
    let mut has_dot = false;
    let mut has_exp = false;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_digit() {
            i += 1;
        } else if b == b'.' && !has_dot && !has_exp {
            has_dot = true;
            i += 1;
        } else if (b == b'e' || b == b'E') && !has_exp {
            has_exp = true;
            i += 1;
            if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
                i += 1;
            }
        } else {
            break;
        }
    }
    let s = std::str::from_utf8(&bytes[start..i]).map_err(|_| parse_err(start, "invalid number"))?;
    if has_dot || has_exp {
        let v: f64 = s.parse().map_err(|_| parse_err(start, "invalid float"))?;
        if !v.is_finite() {
            return Err(parse_err(start, "float literal must be finite"));
        }
        Ok((Tok::Float(v), i))
    } else {
        let v: i64 = s.parse().map_err(|_| parse_err(start, "invalid integer"))?;
        Ok((Tok::Int(v), i))
    }
}

fn read_ident(bytes: &[u8], start: usize) -> (Tok, usize) {
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphanumeric() || b == b'_' {
            i += 1;
        } else {
            break;
        }
    }
    // tokenizer only enters here on ascii alpha/underscore and consumes ascii
    // alnum+underscore, so the slice is guaranteed ascii (and thus utf-8).
    #[expect(clippy::expect_used, reason = "ascii-alnum slice is structurally utf-8")]
    let raw = std::str::from_utf8(&bytes[start..i]).expect("ascii alnum slice is valid utf8");
    let tok = match raw.to_ascii_uppercase().as_str() {
        "AND" => Tok::KwAnd,
        "OR" => Tok::KwOr,
        "NOT" => Tok::KwNot,
        "IN" => Tok::KwIn,
        "LIKE" => Tok::KwLike,
        "IS" => Tok::KwIs,
        "NULL" => Tok::KwNull,
        "TRUE" => Tok::KwTrue,
        "FALSE" => Tok::KwFalse,
        _ => Tok::Ident(raw.to_string()),
    };
    (tok, i)
}

/// Maximum recursive depth across `parse_or` / `parse_and` / `parse_not` /
/// `parse_primary`. Bounds stack growth on adversarial input like `NOT NOT...`
/// or `(((...)))`. 64 levels is well above any sane filter expression.
const MAX_DEPTH: u32 = 64;

struct Parser {
    toks: Vec<Spanned>,
    i: usize,
    end_pos: usize,
    depth: u32,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.i).map(|s| &s.tok)
    }
    fn pos(&self) -> usize {
        self.toks.get(self.i).map_or(self.end_pos, |s| s.pos)
    }
    fn bump(&mut self) -> Option<Spanned> {
        let t = self.toks.get(self.i).cloned();
        if t.is_some() {
            self.i += 1;
        }
        t
    }
    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == Some(t) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, t: &Tok, what: &str) -> Result<(), ExprError> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(parse_err(self.pos(), &format!("expected {what}")))
        }
    }

    fn enter(&mut self) -> Result<(), ExprError> {
        if self.depth >= MAX_DEPTH {
            return Err(ExprError::TooDeep { max: MAX_DEPTH });
        }
        self.depth += 1;
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    fn parse_or(&mut self) -> Result<Expr, ExprError> {
        self.enter()?;
        let r = self.parse_or_inner();
        self.leave();
        r
    }

    fn parse_or_inner(&mut self) -> Result<Expr, ExprError> {
        let mut lhs = self.parse_and()?;
        while self.eat(&Tok::KwOr) {
            let rhs = self.parse_and()?;
            lhs = match lhs {
                Expr::Logic {
                    op: LogicOp::Or,
                    mut args,
                } => {
                    args.push(rhs);
                    Expr::Logic { op: LogicOp::Or, args }
                }
                other => Expr::Logic {
                    op: LogicOp::Or,
                    args: vec![other, rhs],
                },
            };
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr, ExprError> {
        let mut lhs = self.parse_not()?;
        while self.eat(&Tok::KwAnd) {
            let rhs = self.parse_not()?;
            lhs = match lhs {
                Expr::Logic {
                    op: LogicOp::And,
                    mut args,
                } => {
                    args.push(rhs);
                    Expr::Logic { op: LogicOp::And, args }
                }
                other => Expr::Logic {
                    op: LogicOp::And,
                    args: vec![other, rhs],
                },
            };
        }
        Ok(lhs)
    }

    fn parse_not(&mut self) -> Result<Expr, ExprError> {
        self.enter()?;
        let r = if self.eat(&Tok::KwNot) {
            self.parse_not().map(|inner| Expr::Not(Box::new(inner)))
        } else {
            self.parse_predicate()
        };
        self.leave();
        r
    }

    fn parse_predicate(&mut self) -> Result<Expr, ExprError> {
        let lhs = self.parse_primary()?;
        // postfix predicate operators
        match self.peek() {
            Some(Tok::Eq) => {
                self.bump();
                let rhs = self.parse_primary()?;
                Ok(Expr::Cmp {
                    op: CmpOp::Eq,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                })
            }
            Some(Tok::Ne) => {
                self.bump();
                let rhs = self.parse_primary()?;
                Ok(Expr::Cmp {
                    op: CmpOp::Ne,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                })
            }
            Some(Tok::Lt) => {
                self.bump();
                let rhs = self.parse_primary()?;
                Ok(Expr::Cmp {
                    op: CmpOp::Lt,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                })
            }
            Some(Tok::Le) => {
                self.bump();
                let rhs = self.parse_primary()?;
                Ok(Expr::Cmp {
                    op: CmpOp::Le,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                })
            }
            Some(Tok::Gt) => {
                self.bump();
                let rhs = self.parse_primary()?;
                Ok(Expr::Cmp {
                    op: CmpOp::Gt,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                })
            }
            Some(Tok::Ge) => {
                self.bump();
                let rhs = self.parse_primary()?;
                Ok(Expr::Cmp {
                    op: CmpOp::Ge,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                })
            }
            Some(Tok::KwIs) => {
                self.bump();
                let negate = self.eat(&Tok::KwNot);
                self.expect(&Tok::KwNull, "NULL after IS")?;
                if negate {
                    Ok(Expr::IsNotNull(Box::new(lhs)))
                } else {
                    Ok(Expr::IsNull(Box::new(lhs)))
                }
            }
            Some(Tok::KwIn) => {
                self.bump();
                let list = self.parse_literal_list()?;
                Ok(Expr::In {
                    lhs: Box::new(lhs),
                    list,
                })
            }
            Some(Tok::KwLike) => {
                self.bump();
                let pat = self.expect_string("string after LIKE")?;
                Ok(Expr::Like {
                    lhs: Box::new(lhs),
                    pattern: pat,
                })
            }
            Some(Tok::KwNot) => {
                // NOT IN / NOT LIKE
                self.bump();
                match self.peek() {
                    Some(Tok::KwIn) => {
                        self.bump();
                        let list = self.parse_literal_list()?;
                        Ok(Expr::Not(Box::new(Expr::In {
                            lhs: Box::new(lhs),
                            list,
                        })))
                    }
                    Some(Tok::KwLike) => {
                        self.bump();
                        let pat = self.expect_string("string after NOT LIKE")?;
                        Ok(Expr::Not(Box::new(Expr::Like {
                            lhs: Box::new(lhs),
                            pattern: pat,
                        })))
                    }
                    _ => Err(parse_err(self.pos(), "expected IN or LIKE after NOT")),
                }
            }
            _ => Ok(lhs),
        }
    }

    fn parse_literal_list(&mut self) -> Result<Vec<Literal>, ExprError> {
        self.expect(&Tok::LParen, "'(' to open IN list")?;
        let mut out = Vec::new();
        if self.eat(&Tok::RParen) {
            return Ok(out);
        }
        loop {
            out.push(self.parse_literal()?);
            if self.eat(&Tok::Comma) {
                continue;
            }
            self.expect(&Tok::RParen, "')' to close IN list")?;
            break;
        }
        Ok(out)
    }

    fn parse_literal(&mut self) -> Result<Literal, ExprError> {
        let pos = self.pos();
        match self.bump().map(|s| s.tok) {
            Some(Tok::KwNull) => Ok(Literal::Null),
            Some(Tok::KwTrue) => Ok(Literal::Bool(true)),
            Some(Tok::KwFalse) => Ok(Literal::Bool(false)),
            Some(Tok::Int(n)) => Ok(Literal::Int(n)),
            Some(Tok::Float(f)) => Ok(Literal::Float(f)),
            Some(Tok::Str(s)) => Ok(Literal::String(s)),
            _ => Err(parse_err(pos, "expected literal")),
        }
    }

    fn expect_string(&mut self, what: &str) -> Result<String, ExprError> {
        let pos = self.pos();
        match self.bump().map(|s| s.tok) {
            Some(Tok::Str(s)) => Ok(s),
            _ => Err(parse_err(pos, &format!("expected {what}"))),
        }
    }

    fn parse_primary(&mut self) -> Result<Expr, ExprError> {
        self.enter()?;
        let r = self.parse_primary_inner();
        self.leave();
        r
    }

    fn parse_primary_inner(&mut self) -> Result<Expr, ExprError> {
        let pos = self.pos();
        match self.peek().cloned() {
            Some(Tok::LParen) => {
                self.bump();
                let e = self.parse_or()?;
                self.expect(&Tok::RParen, "')'")?;
                // function call would be ident immediately followed by lparen, which would
                // hit this branch via parse_primary on the lparen — but our caller already
                // consumed the ident first. detect that case in parse_predicate is hard;
                // instead, function calls are caught because ident is parsed as primary,
                // and the next token being '(' is rejected here as no postfix matches.
                Ok(e)
            }
            Some(Tok::Ident(name)) => {
                self.bump();
                // reject function calls: ident '('
                if matches!(self.peek(), Some(Tok::LParen)) {
                    return Err(parse_err(self.pos(), "function calls are not allowed"));
                }
                Ok(Expr::Ident(name))
            }
            Some(Tok::KwNull) => {
                self.bump();
                Ok(Expr::Literal(Literal::Null))
            }
            Some(Tok::KwTrue) => {
                self.bump();
                Ok(Expr::Literal(Literal::Bool(true)))
            }
            Some(Tok::KwFalse) => {
                self.bump();
                Ok(Expr::Literal(Literal::Bool(false)))
            }
            Some(Tok::Int(n)) => {
                self.bump();
                Ok(Expr::Literal(Literal::Int(n)))
            }
            Some(Tok::Float(f)) => {
                self.bump();
                Ok(Expr::Literal(Literal::Float(f)))
            }
            Some(Tok::Str(s)) => {
                self.bump();
                Ok(Expr::Literal(Literal::String(s)))
            }
            Some(_) => Err(parse_err(pos, "expected primary expression")),
            None => Err(parse_err(pos, "unexpected end of input")),
        }
    }
}

pub(crate) fn parse(input: &str) -> Result<Expr, ExprError> {
    if input.trim().is_empty() {
        return Err(ExprError::Parse("empty expression".into()));
    }
    let toks = tokenize(input)?;
    let end_pos = input.len();
    let mut p = Parser {
        toks,
        i: 0,
        end_pos,
        depth: 0,
    };
    let expr = p.parse_or()?;
    if p.i < p.toks.len() {
        return Err(parse_err(p.pos(), "trailing tokens after expression"));
    }
    Ok(expr)
}
