use super::ExpressionError;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Token {
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

pub(super) struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    line: usize,
}

impl<'a> Lexer<'a> {
    pub(super) fn new(input: &'a str, line: usize) -> Self {
        Self {
            chars: input.chars().peekable(),
            line,
        }
    }

    pub(super) fn run(&mut self) -> Result<Vec<Token>, ExpressionError> {
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
                '0'..='9' | '-' | '+' => out.push(self.read_number()),
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

    fn read_number(&mut self) -> Token {
        let mut s = String::new();
        while let Some(&c) = self.chars.peek() {
            if c.is_ascii_digit() || c == '.' || c == 'e' || c == 'E' || c == '-' || c == '+' {
                s.push(c);
                self.chars.next();
            } else {
                break;
            }
        }
        Token::Number(s)
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
