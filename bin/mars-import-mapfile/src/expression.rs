//! mapfile-flavoured EXPRESSION parser, lowering to `mars_expr::Expr`.
#![allow(dead_code)]
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
    let expr = parser.parse_expr()?;
    parser.expect_eof()?;
    Ok(expr)
}

// ------------------------------------------------------------------ lexer

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    String(String),
    Number(String),
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    LParen,
    RParen,
    Comma,
    And,
    Or,
    Not,
    In,
    Like,
    Is,
    Null,
    True,
    False,
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
                    match self.chars.peek() {
                        Some(&'>') => {
                            self.chars.next();
                            out.push(Token::Ne);
                        }
                        Some(&'=') => {
                            self.chars.next();
                            out.push(Token::Le);
                        }
                        _ => out.push(Token::Lt),
                    }
                }
                '>' => {
                    self.chars.next();
                    if self.chars.peek() == Some(&'=') {
                        self.chars.next();
                        out.push(Token::Ge);
                    } else {
                        out.push(Token::Gt);
                    }
                }
                '!' => {
                    self.chars.next();
                    if self.chars.peek() == Some(&'=') {
                        self.chars.next();
                        out.push(Token::Ne);
                    } else {
                        out.push(Token::Not);
                    }
                }
                '&' => {
                    self.chars.next();
                    if self.chars.peek() == Some(&'&') {
                        self.chars.next();
                        out.push(Token::And);
                    } else {
                        return Err(ExpressionError::Unsupported {
                            op: "&".to_string(),
                            line: self.line,
                        });
                    }
                }
                '|' => {
                    self.chars.next();
                    if self.chars.peek() == Some(&'|') {
                        self.chars.next();
                        out.push(Token::Or);
                    } else {
                        return Err(ExpressionError::Unsupported {
                            op: "|".to_string(),
                            line: self.line,
                        });
                    }
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
            "LIKE" => Token::Like,
            "IS" => Token::Is,
            "NULL" => Token::Null,
            "TRUE" => Token::True,
            "FALSE" => Token::False,
            // mapserver keyword cmp aliases (case-insensitive)
            "EQ" => Token::Eq,
            "NE" => Token::Ne,
            "LT" => Token::Lt,
            "LE" => Token::Le,
            "GT" => Token::Gt,
            "GE" => Token::Ge,
            _ => {
                return Err(ExpressionError::Unsupported { op: s, line: self.line });
            }
        };
        Ok(tok)
    }
}

// mapfile values like `12-` / `2.5-12` / `0-2.5` are bareword string literals,
// not numbers. fall back to a string when the token doesn't parse cleanly.
fn number_or_string(s: &str) -> Literal {
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

// ---------------------------------------------------------------- parser

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    line: usize,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Token], line: usize) -> Self {
        Self { tokens, pos: 0, line }
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

    fn expect_eof(&self) -> Result<(), ExpressionError> {
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
        let e = parse_mapfile_expression("[geometristatus] <> 'Foreløbig' AND [bygningstype] = 'Drivhus'", 1).unwrap();
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
        let e = parse_mapfile_expression("[vejkategori] IN ('Hovedrute', 'Stor vej')", 1).unwrap();
        assert_eq!(
            e,
            Expr::In {
                lhs: Box::new(Expr::Ident("vejkategori".into())),
                list: vec![Literal::String("Hovedrute".into()), Literal::String("Stor vej".into()),],
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

    // ----------------------------------------- new ops widening parity

    fn cmp(op: CmpOp, lhs: &str, rhs: Expr) -> Expr {
        Expr::Cmp {
            op,
            lhs: Box::new(Expr::Ident(lhs.into())),
            rhs: Box::new(rhs),
        }
    }

    #[test]
    fn numeric_cmp_ops() {
        let lit = |n: i64| Expr::Literal(Literal::Int(n));
        for (src, op) in [
            ("[a] < 5", CmpOp::Lt),
            ("[a] <= 5", CmpOp::Le),
            ("[a] > 5", CmpOp::Gt),
            ("[a] >= 5", CmpOp::Ge),
        ] {
            let e = parse_mapfile_expression(src, 1).unwrap();
            assert_eq!(e, cmp(op, "a", lit(5)), "for input {src}");
        }
    }

    #[test]
    fn bang_eq_is_ne() {
        let e = parse_mapfile_expression("[a] != 5", 1).unwrap();
        assert_eq!(e, cmp(CmpOp::Ne, "a", Expr::Literal(Literal::Int(5))));
    }

    #[test]
    fn keyword_cmp_aliases() {
        let int = |n: i64| Expr::Literal(Literal::Int(n));
        let s = |v: &str| Expr::Literal(Literal::String(v.into()));
        let cases = [
            ("[a] eq 'x'", CmpOp::Eq, s("x")),
            ("[a] ne 'x'", CmpOp::Ne, s("x")),
            ("[a] lt 5", CmpOp::Lt, int(5)),
            ("[a] le 5", CmpOp::Le, int(5)),
            ("[a] gt 5", CmpOp::Gt, int(5)),
            ("[a] ge 5", CmpOp::Ge, int(5)),
        ];
        for (src, op, rhs) in cases {
            let e = parse_mapfile_expression(src, 1).unwrap();
            assert_eq!(e, cmp(op, "a", rhs.clone()), "for input {src}");
        }
    }

    #[test]
    fn c_style_logic_symbols() {
        let int = |n: i64| Expr::Literal(Literal::Int(n));
        let and = parse_mapfile_expression("[a] = 1 && [b] = 2", 1).unwrap();
        assert_eq!(
            and,
            Expr::Logic {
                op: LogicOp::And,
                args: vec![cmp(CmpOp::Eq, "a", int(1)), cmp(CmpOp::Eq, "b", int(2))],
            }
        );
        let or = parse_mapfile_expression("[a] = 1 || [b] = 2", 1).unwrap();
        assert_eq!(
            or,
            Expr::Logic {
                op: LogicOp::Or,
                args: vec![cmp(CmpOp::Eq, "a", int(1)), cmp(CmpOp::Eq, "b", int(2))],
            }
        );
        let bang = parse_mapfile_expression("!([a] = 1)", 1).unwrap();
        assert_eq!(bang, Expr::Not(Box::new(cmp(CmpOp::Eq, "a", int(1)))));
    }

    #[test]
    fn like_pattern() {
        let e = parse_mapfile_expression("[a] LIKE 'foo%'", 1).unwrap();
        assert_eq!(
            e,
            Expr::Like {
                lhs: Box::new(Expr::Ident("a".into())),
                pattern: "foo%".to_string(),
            }
        );
    }

    #[test]
    fn is_null_and_is_not_null() {
        let e = parse_mapfile_expression("[a] IS NULL", 1).unwrap();
        assert_eq!(e, Expr::IsNull(Box::new(Expr::Ident("a".into()))));
        let e = parse_mapfile_expression("[a] IS NOT NULL", 1).unwrap();
        assert_eq!(e, Expr::IsNotNull(Box::new(Expr::Ident("a".into()))));
    }

    #[test]
    fn boolean_literals() {
        let e = parse_mapfile_expression("[active] = TRUE", 1).unwrap();
        assert_eq!(e, cmp(CmpOp::Eq, "active", Expr::Literal(Literal::Bool(true))));
        let e = parse_mapfile_expression("[deleted] = false", 1).unwrap();
        assert_eq!(e, cmp(CmpOp::Eq, "deleted", Expr::Literal(Literal::Bool(false))));
    }

    #[test]
    fn naked_null_literal_rejected() {
        // mars_expr rejects `a = NULL`; mirror that so we never emit DSL
        // that fails to recompile downstream.
        let err = parse_mapfile_expression("[a] = NULL", 1).unwrap_err();
        assert!(matches!(err, ExpressionError::Parse { .. }), "got {err:?}");
    }

    #[test]
    fn no_cmp_chaining() {
        // `a = b = c` would form invalid DSL on round-trip; reject early.
        let err = parse_mapfile_expression("[a] = 1 = 2", 1).unwrap_err();
        assert!(matches!(err, ExpressionError::Parse { .. }), "got {err:?}");
    }

    #[test]
    fn roundtrip_through_mars_expr_parse() {
        // every importer-emitted expression must reparse cleanly through the
        // mars_expr DSL parser, since that is what the YAML pipeline consumes.
        let inputs = [
            "[a] < 5",
            "[a] >= 5",
            "[a] != 5",
            "[a] eq 'x'",
            "[a] gt 5",
            "[a] LIKE 'foo%'",
            "[a] IS NULL",
            "[a] IS NOT NULL",
            "[a] = TRUE",
            "[a] = 1 && [b] = 2",
            "[a] = 1 || [b] = 2",
            "!([a] = 1)",
            "[a] IN (1, 2, 3)",
            "[a] NOT IN ('x', 'y')",
        ];
        for src in inputs {
            let e = parse_mapfile_expression(src, 1).unwrap_or_else(|err| panic!("parse `{src}`: {err}"));
            let emitted = format!("{e}");
            let reparsed = mars_expr::parse(&emitted)
                .unwrap_or_else(|err| panic!("mars_expr can't reparse `{emitted}` (from `{src}`): {err}"));
            assert_eq!(e, reparsed, "ast drift `{src}` -> `{emitted}`");
        }
    }
}
