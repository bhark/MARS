//! Lower a `mars-expr::Expr` to a parameterised SQL `WHERE` fragment.
//!
//! Identifiers are checked against the binding's allowlist
//! (`attributes ∪ {id_column}`) and emitted via `quote_ident`. Values are
//! always parameterised as `$N`; the caller offsets `N` past the spatial
//! params.

use mars_expr::{CmpOp, Expr, Literal, LogicOp};
use mars_source::{SourceBinding, SourceError};

use crate::SqlParam;
use crate::quote::quote_ident;

/// Lower the AST. Returned SQL emits placeholders numbered starting at
/// `$start_index` (so callers that have already-bound parameters can pass the
/// next free slot directly; no post-hoc renumbering is needed).
pub fn lower_to_sql(
    expr: &Expr,
    binding: &SourceBinding,
    start_index: usize,
) -> Result<(String, Vec<SqlParam>), SourceError> {
    let mut ctx = LowerCtx {
        binding,
        params: Vec::new(),
        start_index,
    };
    let sql = lower(expr, &mut ctx)?;
    Ok((sql, ctx.params))
}

struct LowerCtx<'a> {
    binding: &'a SourceBinding,
    params: Vec<SqlParam>,
    start_index: usize,
}

impl<'a> LowerCtx<'a> {
    fn push_param(&mut self, p: SqlParam) -> String {
        self.params.push(p);
        format!("${}", self.start_index + self.params.len() - 1)
    }

    fn check_ident(&self, name: &str) -> Result<String, SourceError> {
        let allowed = self.binding.id_column == name || self.binding.attributes.iter().any(|a| a == name);
        if !allowed {
            return Err(SourceError::UnknownIdent { name: name.to_string() });
        }
        quote_ident(name)
    }
}

fn lower(e: &Expr, ctx: &mut LowerCtx<'_>) -> Result<String, SourceError> {
    match e {
        Expr::Literal(l) => Ok(lower_literal(l, ctx)),
        Expr::Ident(name) => ctx.check_ident(name),
        Expr::Cmp { op, lhs, rhs } => {
            let l = lower(lhs, ctx)?;
            let r = lower(rhs, ctx)?;
            Ok(format!("{l} {} {r}", cmp_sql(*op)))
        }
        Expr::Logic { op, args } => {
            let sep = match op {
                LogicOp::And => " AND ",
                LogicOp::Or => " OR ",
            };
            let mut parts = Vec::with_capacity(args.len());
            for a in args {
                parts.push(lower_logic_arg(*op, a, ctx)?);
            }
            Ok(parts.join(sep))
        }
        Expr::Not(inner) => {
            let s = lower(inner, ctx)?;
            // wrap to make precedence explicit
            Ok(format!("NOT ({s})"))
        }
        Expr::In { lhs, list } => {
            let l = lower(lhs, ctx)?;
            if list.is_empty() {
                // empty IN list is always false; emit a constant rather than
                // invalid sql `IN ()`.
                return Ok(format!("({l} IN (NULL) AND FALSE)"));
            }
            let mut placeholders = Vec::with_capacity(list.len());
            for lit in list {
                placeholders.push(lower_literal(lit, ctx));
            }
            Ok(format!("{l} IN ({})", placeholders.join(", ")))
        }
        Expr::Like { lhs, pattern } => {
            let l = lower(lhs, ctx)?;
            let p = ctx.push_param(SqlParam::Text(pattern.clone()));
            Ok(format!("{l} LIKE {p}"))
        }
        Expr::IsNull(inner) => {
            let s = lower(inner, ctx)?;
            Ok(format!("{s} IS NULL"))
        }
        Expr::IsNotNull(inner) => {
            let s = lower(inner, ctx)?;
            Ok(format!("{s} IS NOT NULL"))
        }
    }
}

fn lower_logic_arg(parent: LogicOp, e: &Expr, ctx: &mut LowerCtx<'_>) -> Result<String, SourceError> {
    // parens around mixed-precedence children mirror the Display impl
    let needs_paren = matches!((parent, e), (LogicOp::And, Expr::Logic { op: LogicOp::Or, .. }));
    let s = lower(e, ctx)?;
    if needs_paren { Ok(format!("({s})")) } else { Ok(s) }
}

fn lower_literal(l: &Literal, ctx: &mut LowerCtx<'_>) -> String {
    match l {
        // NULL is a keyword, not a parameter — postgres can't bind a typed
        // NULL via `$N` in arbitrary positions without explicit casts.
        Literal::Null => "NULL".to_string(),
        Literal::Bool(b) => ctx.push_param(SqlParam::Bool(*b)),
        Literal::Int(i) => ctx.push_param(SqlParam::Int(*i)),
        Literal::Float(f) => ctx.push_param(SqlParam::Float(*f)),
        Literal::String(s) => ctx.push_param(SqlParam::Text(s.clone())),
    }
}

fn cmp_sql(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "=",
        CmpOp::Ne => "<>",
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use mars_expr::parse;
    use mars_source::SourceCollectionId;
    use mars_types::CrsCode;

    fn binding(attrs: &[&str], id: &str) -> SourceBinding {
        SourceBinding::new(
            SourceCollectionId::new("c"),
            "public",
            "t",
            "geom",
            id,
            attrs.iter().map(|s| (*s).to_string()).collect(),
            CrsCode::new("EPSG:25832"),
        )
        .unwrap()
    }

    #[test]
    fn lowers_three_clause_filter() {
        let e = parse("ttype = 'forest' AND area >= 1000").unwrap();
        let b = binding(&["ttype", "area"], "gid");
        let (sql, params) = lower_to_sql(&e, &b, 1).unwrap();
        assert_eq!(sql, "\"ttype\" = $1 AND \"area\" >= $2");
        assert_eq!(params.len(), 2);
        assert!(matches!(&params[0], SqlParam::Text(s) if s == "forest"));
        assert!(matches!(&params[1], SqlParam::Int(1000)));
    }

    #[test]
    fn rejects_unknown_ident() {
        let e = parse("evil = 1").unwrap();
        let b = binding(&["ttype"], "gid");
        let r = lower_to_sql(&e, &b, 1);
        assert!(matches!(r, Err(SourceError::UnknownIdent { name }) if name == "evil"));
    }

    #[test]
    fn lowers_in_list() {
        let e = parse("kind IN ('a','b')").unwrap();
        let b = binding(&["kind"], "gid");
        let (sql, params) = lower_to_sql(&e, &b, 1).unwrap();
        assert_eq!(sql, "\"kind\" IN ($1, $2)");
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn lowers_like() {
        let e = parse("name LIKE 'foo%'").unwrap();
        let b = binding(&["name"], "gid");
        let (sql, params) = lower_to_sql(&e, &b, 1).unwrap();
        assert_eq!(sql, "\"name\" LIKE $1");
        assert!(matches!(&params[0], SqlParam::Text(s) if s == "foo%"));
    }

    #[test]
    fn lowers_is_not_null() {
        let e = parse("name IS NOT NULL").unwrap();
        let b = binding(&["name"], "gid");
        let (sql, params) = lower_to_sql(&e, &b, 1).unwrap();
        assert_eq!(sql, "\"name\" IS NOT NULL");
        assert!(params.is_empty());
    }

    #[test]
    fn lowers_not_group() {
        let e = parse("NOT (a = 1)").unwrap();
        let b = binding(&["a"], "gid");
        let (sql, params) = lower_to_sql(&e, &b, 1).unwrap();
        assert_eq!(sql, "NOT (\"a\" = $1)");
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn id_column_in_allowlist() {
        let e = parse("gid = 7").unwrap();
        let b = binding(&[], "gid");
        let (sql, params) = lower_to_sql(&e, &b, 1).unwrap();
        assert_eq!(sql, "\"gid\" = $1");
        assert!(matches!(&params[0], SqlParam::Int(7)));
    }

    #[test]
    fn null_literal_not_parameterised() {
        let e = parse("a = NULL").unwrap();
        let b = binding(&["a"], "gid");
        let (sql, params) = lower_to_sql(&e, &b, 1).unwrap();
        assert_eq!(sql, "\"a\" = NULL");
        assert!(params.is_empty());
    }
}
