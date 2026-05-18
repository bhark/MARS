use crate::ExprError;

use super::parse_err;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Tok {
    LParen,
    RParen,
    Comma,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    // postgres-style regex match operators
    Tilde,
    TildeStar,
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
pub(super) struct Spanned {
    pub(super) tok: Tok,
    pub(super) pos: usize,
}

pub(super) fn tokenize(input: &str) -> Result<Vec<Spanned>, ExprError> {
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
            b'~' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                    out.push(Spanned {
                        tok: Tok::TildeStar,
                        pos,
                    });
                    i += 2;
                } else {
                    out.push(Spanned { tok: Tok::Tilde, pos });
                    i += 1;
                }
            }
            b'+' | b'*' | b'/' | b'%' | b'^' | b'&' | b'|' | b'?' | b'@' | b'#' | b'$' | b':' | b';' => {
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

fn read_string(bytes: &[u8], start: usize) -> Result<(String, usize), ExprError> {
    // walk by char_indices on the post-quote slice so multi-byte utf-8 (e.g.
    // danish 'ø' in 'Foreløbig') is preserved instead of being byte-as-char'd.
    let body_start = start + 1;
    let s =
        std::str::from_utf8(&bytes[body_start..]).map_err(|_| parse_err(start, "invalid utf-8 in string literal"))?;
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
