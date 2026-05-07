//! Identifier quoting. Bare-ident only; dotted names rejected so callers must
//! split schema and table themselves and quote each piece.

use mars_source::SourceError;

/// Quote a bare SQL identifier (column / table / schema). Rejects dotted,
/// empty, and NUL-bearing inputs so the result is always safe to splice
/// directly into generated SQL.
pub(crate) fn quote_ident(name: &str) -> Result<String, SourceError> {
    if name.is_empty() {
        return Err(SourceError::backend_msg("quote_ident", "empty identifier"));
    }
    if name.contains('.') {
        return Err(SourceError::backend_msg(
            "quote_ident",
            format!("dotted identifier rejected: {name}"),
        ));
    }
    if name.contains('\0') {
        return Err(SourceError::backend_msg("quote_ident", "identifier contains NUL"));
    }
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    for ch in name.chars() {
        if ch == '"' {
            out.push_str("\"\"");
        } else {
            out.push(ch);
        }
    }
    out.push('"');
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn quotes_plain() {
        assert_eq!(quote_ident("foo").unwrap(), "\"foo\"");
    }

    #[test]
    fn doubles_embedded_quote() {
        assert_eq!(quote_ident("foo\"bar").unwrap(), "\"foo\"\"bar\"");
    }

    #[test]
    fn rejects_dotted() {
        assert!(matches!(quote_ident("a.b"), Err(SourceError::Backend { .. })));
    }

    #[test]
    fn rejects_nul() {
        assert!(matches!(quote_ident("a\0b"), Err(SourceError::Backend { .. })));
    }

    #[test]
    fn rejects_empty() {
        assert!(matches!(quote_ident(""), Err(SourceError::Backend { .. })));
    }
}
