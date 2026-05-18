use crate::{CmpOp, Expr, ExprError, Literal, LogicOp};

use super::lexer::{Spanned, Tok};
use super::{MAX_DEPTH, parse_err};

/// `=`/`!=` against `NULL` is silently always-NULL in SQL - never matches.
/// reject at parse time and steer authors to `IS NULL` / `IS NOT NULL`.
fn reject_null_compare(lhs: &Expr, rhs: &Expr, op: &str) -> Result<(), ExprError> {
    if matches!(lhs, Expr::Literal(Literal::Null)) || matches!(rhs, Expr::Literal(Literal::Null)) {
        return Err(ExprError::Parse(format!(
            "{op} NULL never matches in SQL; use IS NULL / IS NOT NULL"
        )));
    }
    Ok(())
}

pub(super) struct Parser {
    toks: Vec<Spanned>,
    i: usize,
    end_pos: usize,
    depth: u32,
}

impl Parser {
    pub(super) fn new(toks: Vec<Spanned>, end_pos: usize) -> Self {
        Self {
            toks,
            i: 0,
            end_pos,
            depth: 0,
        }
    }

    pub(super) fn at_end(&self) -> bool {
        self.i >= self.toks.len()
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.i).map(|s| &s.tok)
    }
    pub(super) fn pos(&self) -> usize {
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

    pub(super) fn parse_or(&mut self) -> Result<Expr, ExprError> {
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
                reject_null_compare(&lhs, &rhs, "=")?;
                Ok(Expr::Cmp {
                    op: CmpOp::Eq,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                })
            }
            Some(Tok::Ne) => {
                self.bump();
                let rhs = self.parse_primary()?;
                reject_null_compare(&lhs, &rhs, "!=")?;
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
            Some(Tok::Tilde) => {
                self.bump();
                let pat = self.expect_string("string after ~")?;
                Ok(Expr::Regex {
                    lhs: Box::new(lhs),
                    pattern: pat,
                    case_insensitive: false,
                })
            }
            Some(Tok::TildeStar) => {
                self.bump();
                let pat = self.expect_string("string after ~*")?;
                Ok(Expr::Regex {
                    lhs: Box::new(lhs),
                    pattern: pat,
                    case_insensitive: true,
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
                // hit this branch via parse_primary on the lparen - but our caller already
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
