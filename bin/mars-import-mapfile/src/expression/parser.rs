use mars_expr::{CmpOp, Expr, Literal, LogicOp};

use super::lexer::Token;
use super::{ExpressionError, number_or_string};

pub(super) struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    line: usize,
}

impl<'a> Parser<'a> {
    pub(super) fn new(tokens: &'a [Token], line: usize) -> Self {
        Self { tokens, pos: 0, line }
    }

    pub(super) fn parse_expr(&mut self) -> Result<Expr, ExpressionError> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr, ExpressionError> {
        let mut args = vec![self.parse_and()?];
        while self.eat(&Token::Or) {
            args.push(self.parse_and()?);
        }
        Ok(if args.len() == 1 {
            args.swap_remove(0)
        } else {
            Expr::Logic { op: LogicOp::Or, args }
        })
    }

    fn parse_and(&mut self) -> Result<Expr, ExpressionError> {
        let mut args = vec![self.parse_not()?];
        while self.eat(&Token::And) {
            args.push(self.parse_not()?);
        }
        Ok(if args.len() == 1 {
            args.swap_remove(0)
        } else {
            Expr::Logic { op: LogicOp::And, args }
        })
    }

    fn parse_not(&mut self) -> Result<Expr, ExpressionError> {
        if self.eat(&Token::Not) {
            let inner = self.parse_not()?;
            Ok(Expr::Not(Box::new(inner)))
        } else {
            self.parse_predicate()
        }
    }

    /// one operand plus an optional postfix predicate or comparison.
    /// no chaining: `a = b = c` is a parse error.
    fn parse_predicate(&mut self) -> Result<Expr, ExpressionError> {
        if self.eat(&Token::LParen) {
            let e = self.parse_expr()?;
            self.expect(&Token::RParen)?;
            return Ok(e);
        }

        let lhs = self.parse_operand()?;

        match self.current().cloned() {
            Some(Token::Is) => {
                self.pos += 1;
                let negate = self.eat(&Token::Not);
                self.expect(&Token::Null)?;
                let inner = Box::new(lhs);
                Ok(if negate {
                    Expr::IsNotNull(inner)
                } else {
                    Expr::IsNull(inner)
                })
            }
            Some(Token::Not) => {
                // only valid as `NOT IN (...)` here; bare NOT is a prefix and
                // handled by parse_not before we arrive.
                self.pos += 1;
                self.expect(&Token::In)?;
                let list = self.parse_in_list()?;
                Ok(Expr::Not(Box::new(Expr::In {
                    lhs: Box::new(lhs),
                    list,
                })))
            }
            Some(Token::In) => {
                self.pos += 1;
                let list = self.parse_in_list()?;
                Ok(Expr::In {
                    lhs: Box::new(lhs),
                    list,
                })
            }
            Some(Token::Like) => {
                self.pos += 1;
                let pattern = self.parse_string_literal()?;
                Ok(Expr::Like {
                    lhs: Box::new(lhs),
                    pattern,
                })
            }
            Some(tok) => match cmp_op_for(&tok) {
                Some(op) => {
                    self.pos += 1;
                    let rhs = self.parse_operand()?;
                    Ok(Expr::Cmp {
                        op,
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                    })
                }
                None => Ok(lhs),
            },
            None => Ok(lhs),
        }
    }

    fn parse_operand(&mut self) -> Result<Expr, ExpressionError> {
        match self.current().cloned() {
            Some(Token::Ident(s)) => {
                self.pos += 1;
                Ok(Expr::Ident(s))
            }
            Some(Token::String(s)) => {
                self.pos += 1;
                Ok(Expr::Literal(Literal::String(s)))
            }
            Some(Token::Number(s)) => {
                self.pos += 1;
                Ok(Expr::Literal(number_or_string(&s)))
            }
            Some(Token::True) => {
                self.pos += 1;
                Ok(Expr::Literal(Literal::Bool(true)))
            }
            Some(Token::False) => {
                self.pos += 1;
                Ok(Expr::Literal(Literal::Bool(false)))
            }
            Some(ref t) => Err(ExpressionError::Parse {
                msg: format!("unexpected token {t:?}"),
                line: self.line,
            }),
            None => Err(ExpressionError::Parse {
                msg: "unexpected end of expression".to_string(),
                line: self.line,
            }),
        }
    }

    fn parse_in_list(&mut self) -> Result<Vec<Literal>, ExpressionError> {
        self.expect(&Token::LParen)?;
        let mut list = Vec::new();
        if !self.at(&Token::RParen) {
            loop {
                list.push(self.parse_literal()?);
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
        }
        self.expect(&Token::RParen)?;
        Ok(list)
    }

    fn parse_literal(&mut self) -> Result<Literal, ExpressionError> {
        match self.current().cloned() {
            Some(Token::String(s)) => {
                self.pos += 1;
                Ok(Literal::String(s))
            }
            Some(Token::Number(s)) => {
                self.pos += 1;
                Ok(number_or_string(&s))
            }
            Some(Token::True) => {
                self.pos += 1;
                Ok(Literal::Bool(true))
            }
            Some(Token::False) => {
                self.pos += 1;
                Ok(Literal::Bool(false))
            }
            Some(ref t) => Err(ExpressionError::Parse {
                msg: format!("expected literal, got {t:?}"),
                line: self.line,
            }),
            None => Err(ExpressionError::Parse {
                msg: "unexpected end of expression".to_string(),
                line: self.line,
            }),
        }
    }

    fn parse_string_literal(&mut self) -> Result<String, ExpressionError> {
        match self.current().cloned() {
            Some(Token::String(s)) => {
                self.pos += 1;
                Ok(s)
            }
            Some(ref t) => Err(ExpressionError::Parse {
                msg: format!("expected string literal, got {t:?}"),
                line: self.line,
            }),
            None => Err(ExpressionError::Parse {
                msg: "unexpected end of expression".to_string(),
                line: self.line,
            }),
        }
    }

    fn current(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn eat(&mut self, expected: &Token) -> bool {
        if self.at(expected) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn at(&self, expected: &Token) -> bool {
        self.current() == Some(expected)
    }

    fn expect(&mut self, expected: &Token) -> Result<(), ExpressionError> {
        if self.eat(expected) {
            Ok(())
        } else {
            Err(ExpressionError::Parse {
                msg: format!("expected {expected:?}, got {:?}", self.current()),
                line: self.line,
            })
        }
    }

    pub(super) fn expect_eof(&self) -> Result<(), ExpressionError> {
        match self.current() {
            Some(Token::Eof) | None => Ok(()),
            Some(t) => Err(ExpressionError::Parse {
                msg: format!("trailing tokens after expression, starting at {t:?}"),
                line: self.line,
            }),
        }
    }
}

fn cmp_op_for(tok: &Token) -> Option<CmpOp> {
    Some(match tok {
        Token::Eq => CmpOp::Eq,
        Token::Ne => CmpOp::Ne,
        Token::Lt => CmpOp::Lt,
        Token::Le => CmpOp::Le,
        Token::Gt => CmpOp::Gt,
        Token::Ge => CmpOp::Ge,
        _ => return None,
    })
}
