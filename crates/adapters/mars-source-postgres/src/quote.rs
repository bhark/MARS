//! Identifier quoting and FROM-target rendering. `quote_ident` is bare-ident
//! only; dotted names are rejected. Callers shape a binding's opaque `from`
//! locator into a spliceable FROM target through [`render_from_target`], which
//! dispatches on whether the locator is a `schema.table` reference or an
//! inline `(SELECT ...)` subquery (view-shaped sql: binding).

use mars_source::SourceError;

/// Split a `schema.table` locator into postgres `(schema, table)`. Mirrors the
/// config-side convention: single-segment names route to `"public"`.
fn split_from(from: &str) -> (&str, &str) {
    match from.split_once('.') {
        Some((s, t)) => (s, t),
        None => ("public", from),
    }
}

/// Render a binding's opaque `from` locator as a spliceable postgres FROM
/// target. Two shapes:
/// - inline SELECT (locator starts with `(`): returned verbatim with a stable
///   `_mars_src` alias so columns the subquery projects remain referenceable
///   by name (`id`, geometry column, attribute columns) downstream.
/// - table reference (`schema.table` or `table`): split and each identifier
///   quoted, returning `"schema"."table"`.
pub(crate) fn render_from_target(from: &str) -> Result<String, SourceError> {
    if from.starts_with('(') {
        return Ok(format!("{from} AS _mars_src"));
    }
    let (schema, table) = split_from(from);
    let schema_q = quote_ident(schema)?;
    let table_q = quote_ident(table)?;
    Ok(format!("{schema_q}.{table_q}"))
}

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

    #[test]
    fn split_from_dotted() {
        assert_eq!(split_from("public.roads"), ("public", "roads"));
        assert_eq!(
            split_from("geo.administrative.regions"),
            ("geo", "administrative.regions")
        );
    }

    #[test]
    fn split_from_defaults_to_public() {
        assert_eq!(split_from("roads"), ("public", "roads"));
    }

    #[test]
    fn render_from_target_table_form() {
        assert_eq!(render_from_target("public.roads").unwrap(), "\"public\".\"roads\"");
        assert_eq!(render_from_target("roads").unwrap(), "\"public\".\"roads\"");
    }

    #[test]
    fn render_from_target_subquery_form() {
        let from = "(SELECT id, geom, name FROM public.points)";
        assert_eq!(
            render_from_target(from).unwrap(),
            "(SELECT id, geom, name FROM public.points) AS _mars_src"
        );
    }

    #[test]
    fn render_from_target_rejects_malformed_table() {
        assert!(matches!(render_from_target(""), Err(SourceError::Backend { .. })));
    }
}
