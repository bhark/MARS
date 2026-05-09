//! mapfile-flavoured EXPRESSION parser, lowering to `mars_expr::Expr`.
#![allow(dead_code)]
//!
//! supported v1 subset: =, <>, IN, NOT IN, AND, OR, attribute [name] quoting.
//! anything outside this set → `ExpressionError::Unsupported` so the caller
//! can emit a # TODO: hand-translate comment + warning.

use mars_expr::{CmpOp, Expr, Literal, LogicOp};

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
    parser.parse_expr()
}

// ------------------------------------------------------------------ lexer

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    String(String),
    Number(String),
    Eq,
    Ne,
    LParen,
    RParen,
    Comma,
    And,
    Or,
    Not,
    In,
    Eof,
}

struct Lexer<'a> {
    input: &'a str,
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    line: usize,
}

impl<'a> Lexer<'a> {
    fn new(input: &'a str, line: usize) -> Self {
        Self {
            input,
            chars: input.chars().peekable(),
            line,
        }
    }

    fn run(&mut self) -> Result<Vec<Token>, ExpressionError> {
        let mut out = Vec::new();
        loop {
            self.skip_ws();
            let Some(ch) = self.chars.peek().copied() else {
                out.push(Token::Eof);
                return Ok(out);
            };
            match ch {
                '[' => out.push(self.read_bracketed_ident()?),
                '\'' => out.push(self.read_single_quoted_string()?),
                '"' => out.push(self.read_double_quoted_string()?),
                '(' => {
                    self.chars.next();
                    out.push(Token::LParen);
                }
                ')' => {
                    self.chars.next();
                    out.push(Token::RParen);
                }
                ',' => {
                    self.chars.next();
                    out.push(Token::Comma);
                }
                '=' => {
                    self.chars.next();
                    out.push(Token::Eq);
                }
                '<' => {
                    self.chars.next();
                    if self.chars.peek() == Some(&'>') {
                        self.chars.next();
                        out.push(Token::Ne);
                    } else {
                        return Err(ExpressionError::Unsupported {
                            op: "<".to_string(),
                            line: self.line,
                        });
                    }
                }
                '>' => {
                    self.chars.next();
                    return Err(ExpressionError::Unsupported {
                        op: ">".to_string(),
                        line: self.line,
                    });
                }
                '0'..='9' | '-' | '+' => out.push(self.read_number()?),
                _ if ch.is_alphabetic() => out.push(self.read_word()?),
                _ => {
                    return Err(ExpressionError::Unsupported {
                        op: ch.to_string(),
                        line: self.line,
                    });
                }
            }
        }
    }

    fn skip_ws(&mut self) {
        while let Some(&c) = self.chars.peek() {
            if c.is_whitespace() {
                self.chars.next();
            } else {
                break;
            }
        }
    }

    fn read_bracketed_ident(&mut self) -> Result<Token, ExpressionError> {
        self.chars.next(); // '['
        let mut s = String::new();
        loop {
            match self.chars.next() {
                Some(']') => return Ok(Token::Ident(s)),
                Some(c) => s.push(c),
                None => {
                    return Err(ExpressionError::Parse {
                        msg: "unclosed [identifier".to_string(),
                        line: self.line,
                    });
                }
            }
        }
    }

    fn read_single_quoted_string(&mut self) -> Result<Token, ExpressionError> {
        self.chars.next(); // '\''
        let mut s = String::new();
        loop {
            match self.chars.next() {
                Some('\'') => return Ok(Token::String(s)),
                Some(c) => s.push(c),
                None => {
                    return Err(ExpressionError::Parse {
                        msg: "unclosed 'string".to_string(),
                        line: self.line,
                    });
                }
            }
        }
    }

    fn read_double_quoted_string(&mut self) -> Result<Token, ExpressionError> {
        self.chars.next(); // '"'
        let mut s = String::new();
        loop {
            match self.chars.next() {
                Some('"') => return Ok(Token::String(s)),
                Some(c) => s.push(c),
                None => {
                    return Err(ExpressionError::Parse {
                        msg: "unclosed \"string".to_string(),
                        line: self.line,
                    });
                }
            }
        }
    }

    fn read_number(&mut self) -> Result<Token, ExpressionError> {
        let mut s = String::new();
        while let Some(&c) = self.chars.peek() {
            if c.is_ascii_digit() || c == '.' || c == 'e' || c == 'E' || c == '-' || c == '+' {
                s.push(c);
                self.chars.next();
            } else {
                break;
            }
        }
        Ok(Token::Number(s))
    }

    fn read_word(&mut self) -> Result<Token, ExpressionError> {
        let mut s = String::new();
        while let Some(&c) = self.chars.peek() {
            if c.is_alphanumeric() || c == '_' || c == '/' || c == '-' {
                s.push(c);
                self.chars.next();
            } else {
                break;
            }
        }
        let up = s.to_ascii_uppercase();
        let tok = match up.as_str() {
            "AND" => Token::And,
            "OR" => Token::Or,
            "NOT" => Token::Not,
            "IN" => Token::In,
            _ => {
                return Err(ExpressionError::Unsupported {
                    op: s,
                    line: self.line,
                });
            }
        };
        Ok(tok)
    }
}

// ---------------------------------------------------------------- parser

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    line: usize,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Token], line: usize) -> Self {
        Self {
            tokens,
            pos: 0,
            line,
        }
    }

    fn parse_expr(&mut self) -> Result<Expr, ExpressionError> {
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
            Expr::Logic {
                op: LogicOp::Or,
                args,
            }
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
            Expr::Logic {
                op: LogicOp::And,
                args,
            }
        })
    }

    fn parse_not(&mut self) -> Result<Expr, ExpressionError> {
        if self.eat(&Token::Not) {
            let inner = self.parse_not()?;
            Ok(Expr::Not(Box::new(inner)))
        } else {
            self.parse_in()
        }
    }

    fn parse_in(&mut self) -> Result<Expr, ExpressionError> {
        let lhs = self.parse_primary()?;
        let not = self.eat(&Token::Not);
        if self.eat(&Token::In) {
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
            let inner = Expr::In {
                lhs: Box::new(lhs),
                list,
            };
            if not {
                Ok(Expr::Not(Box::new(inner)))
            } else {
                Ok(inner)
            }
        } else if not {
            Err(ExpressionError::Parse {
                msg: "NOT without IN".to_string(),
                line: self.line,
            })
        } else {
            Ok(lhs)
        }
    }

    fn parse_primary(&mut self) -> Result<Expr, ExpressionError> {
        if self.eat(&Token::LParen) {
            let e = self.parse_expr()?;
            self.expect(&Token::RParen)?;
            Ok(e)
        } else {
            self.parse_comparison()
        }
    }

    fn parse_comparison(&mut self) -> Result<Expr, ExpressionError> {
        let lhs = self.parse_operand()?;
        if self.eat(&Token::Eq) {
            let rhs = self.parse_operand()?;
            Ok(Expr::Cmp {
                op: CmpOp::Eq,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            })
        } else if self.eat(&Token::Ne) {
            let rhs = self.parse_operand()?;
            Ok(Expr::Cmp {
                op: CmpOp::Ne,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            })
        } else {
            Ok(lhs)
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
                if let Ok(n) = s.parse::<i64>() {
                    Ok(Expr::Literal(Literal::Int(n)))
                } else if let Ok(f) = s.parse::<f64>() {
                    if f.is_finite() {
                        Ok(Expr::Literal(Literal::Float(f)))
                    } else {
                        Err(ExpressionError::Parse {
                            msg: format!("non-finite number {s}"),
                            line: self.line,
                        })
                    }
                } else {
                    Err(ExpressionError::Parse {
                        msg: format!("invalid number {s}"),
                        line: self.line,
                    })
                }
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

    fn parse_literal(&mut self) -> Result<Literal, ExpressionError> {
        match self.current().cloned() {
            Some(Token::String(s)) => {
                self.pos += 1;
                Ok(Literal::String(s))
            }
            Some(Token::Number(s)) => {
                self.pos += 1;
                if let Ok(n) = s.parse::<i64>() {
                    Ok(Literal::Int(n))
                } else if let Ok(f) = s.parse::<f64>() {
                    if f.is_finite() {
                        Ok(Literal::Float(f))
                    } else {
                        Err(ExpressionError::Parse {
                            msg: format!("non-finite number {s}"),
                            line: self.line,
                        })
                    }
                } else {
                    Err(ExpressionError::Parse {
                        msg: format!("invalid number {s}"),
                        line: self.line,
                    })
                }
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
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn attr_eq_string() {
        let e = parse_mapfile_expression("[bygningstype] = 'Drivhus'", 1).unwrap();
        assert_eq!(
            e,
            Expr::Cmp {
                op: CmpOp::Eq,
                lhs: Box::new(Expr::Ident("bygningstype".into())),
                rhs: Box::new(Expr::Literal(Literal::String("Drivhus".into()))),
            }
        );
    }

    #[test]
    fn ne_and_eq() {
        let e = parse_mapfile_expression(
            "[geometristatus] <> 'Foreløbig' AND [bygningstype] = 'Drivhus'",
            1,
        )
        .unwrap();
        assert_eq!(
            e,
            Expr::Logic {
                op: LogicOp::And,
                args: vec![
                    Expr::Cmp {
                        op: CmpOp::Ne,
                        lhs: Box::new(Expr::Ident("geometristatus".into())),
                        rhs: Box::new(Expr::Literal(Literal::String("Foreløbig".into()))),
                    },
                    Expr::Cmp {
                        op: CmpOp::Eq,
                        lhs: Box::new(Expr::Ident("bygningstype".into())),
                        rhs: Box::new(Expr::Literal(Literal::String("Drivhus".into()))),
                    },
                ],
            }
        );
    }

    #[test]
    fn in_list() {
        let e =
            parse_mapfile_expression("[vejkategori] IN ('Hovedrute', 'Stor vej')", 1).unwrap();
        assert_eq!(
            e,
            Expr::In {
                lhs: Box::new(Expr::Ident("vejkategori".into())),
                list: vec![
                    Literal::String("Hovedrute".into()),
                    Literal::String("Stor vej".into()),
                ],
            }
        );
    }

    #[test]
    fn not_in_list() {
        let e = parse_mapfile_expression("[v] NOT IN ('a', 'b')", 1).unwrap();
        assert_eq!(
            e,
            Expr::Not(Box::new(Expr::In {
                lhs: Box::new(Expr::Ident("v".into())),
                list: vec![Literal::String("a".into()), Literal::String("b".into())],
            }))
        );
    }

    #[test]
    fn unsupported_operator_is_typed() {
        let err = parse_mapfile_expression("[a] =~ '/regex/'", 5).unwrap_err();
        assert_eq!(
            err,
            ExpressionError::Unsupported {
                op: "~".to_string(),
                line: 5,
            }
        );
    }

    #[test]
    fn unsupported_function_call() {
        let err = parse_mapfile_expression("func(x)", 2).unwrap_err();
        assert_eq!(
            err,
            ExpressionError::Unsupported {
                op: "func".to_string(),
                line: 2,
            }
        );
    }

    #[test]
    fn unsupported_lt_gt() {
        let err = parse_mapfile_expression("[a] < 5", 1).unwrap_err();
        assert_eq!(
            err,
            ExpressionError::Unsupported {
                op: "<".to_string(),
                line: 1,
            }
        );
    }

    #[test]
    fn or_chain() {
        let e = parse_mapfile_expression("[a] = '1' OR [b] = '2' OR [c] = '3'", 1).unwrap();
        assert_eq!(
            e,
            Expr::Logic {
                op: LogicOp::Or,
                args: vec![
                    Expr::Cmp {
                        op: CmpOp::Eq,
                        lhs: Box::new(Expr::Ident("a".into())),
                        rhs: Box::new(Expr::Literal(Literal::String("1".into()))),
                    },
                    Expr::Cmp {
                        op: CmpOp::Eq,
                        lhs: Box::new(Expr::Ident("b".into())),
                        rhs: Box::new(Expr::Literal(Literal::String("2".into()))),
                    },
                    Expr::Cmp {
                        op: CmpOp::Eq,
                        lhs: Box::new(Expr::Ident("c".into())),
                        rhs: Box::new(Expr::Literal(Literal::String("3".into()))),
                    },
                ],
            }
        );
    }

    #[test]
    fn parens_grouping() {
        let e = parse_mapfile_expression("([a] = '1' OR [b] = '2') AND [c] = '3'", 1).unwrap();
        assert_eq!(
            e,
            Expr::Logic {
                op: LogicOp::And,
                args: vec![
                    Expr::Logic {
                        op: LogicOp::Or,
                        args: vec![
                            Expr::Cmp {
                                op: CmpOp::Eq,
                                lhs: Box::new(Expr::Ident("a".into())),
                                rhs: Box::new(Expr::Literal(Literal::String("1".into()))),
                            },
                            Expr::Cmp {
                                op: CmpOp::Eq,
                                lhs: Box::new(Expr::Ident("b".into())),
                                rhs: Box::new(Expr::Literal(Literal::String("2".into()))),
                            },
                        ],
                    },
                    Expr::Cmp {
                        op: CmpOp::Eq,
                        lhs: Box::new(Expr::Ident("c".into())),
                        rhs: Box::new(Expr::Literal(Literal::String("3".into()))),
                    },
                ],
            }
        );
    }

    #[test]
    fn number_literal() {
        let e = parse_mapfile_expression("[x] = 42", 1).unwrap();
        assert_eq!(
            e,
            Expr::Cmp {
                op: CmpOp::Eq,
                lhs: Box::new(Expr::Ident("x".into())),
                rhs: Box::new(Expr::Literal(Literal::Int(42))),
            }
        );
    }

    #[test]
    fn float_literal() {
        let e = parse_mapfile_expression("[x] = 2.5", 1).unwrap();
        assert_eq!(
            e,
            Expr::Cmp {
                op: CmpOp::Eq,
                lhs: Box::new(Expr::Ident("x".into())),
                rhs: Box::new(Expr::Literal(Literal::Float(2.5))),
            }
        );
    }

    #[test]
    fn quoted_string_inside_double_quotes() {
        // mapfile: EXPRESSION "[a] = 'hello'"
        let e = parse_mapfile_expression("[a] = 'hello'", 1).unwrap();
        assert_eq!(
            e,
            Expr::Cmp {
                op: CmpOp::Eq,
                lhs: Box::new(Expr::Ident("a".into())),
                rhs: Box::new(Expr::Literal(Literal::String("hello".into()))),
            }
        );
    }

    #[test]
    fn empty_in_list() {
        let e = parse_mapfile_expression("[x] IN ()", 1).unwrap();
        assert!(matches!(
            e,
            Expr::In { list, .. } if list.is_empty()
        ));
    }
}
