//! Per-call SQL builder for `Source::fetch_cell`.

use bytes::Bytes;
use deadpool_postgres::Pool;
use mars_expr::Expr;
use mars_source::{AttrValue, RowBytes, SourceBinding, SourceError};
use mars_types::Bbox;
use tokio_postgres::types::{ToSql, Type};

use crate::SqlParam;
use crate::lower::lower_to_sql;
use crate::quote::quote_ident;

/// Build the fetch query, run it, and decode rows.
pub(crate) async fn fetch_cell(
    pool: &Pool,
    binding: &SourceBinding,
    bbox: Bbox,
    filter: Option<&Expr>,
) -> Result<Vec<RowBytes>, SourceError> {
    let srid = parse_srid(binding.crs.as_str())?;
    let (sql, params) = build_query(binding, bbox, srid, filter)?;
    let client = pool
        .get()
        .await
        .map_err(|e| SourceError::Backend(format!("pool: {e}")))?;
    let stmt = client
        .prepare(&sql)
        .await
        .map_err(|e| SourceError::Backend(format!("prepare: {e}")))?;

    let pg_params: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
    let rows = client
        .query(&stmt, &pg_params)
        .await
        .map_err(|e| SourceError::Backend(format!("query: {e}")))?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(decode_row(&row, binding)?);
    }
    Ok(out)
}

/// SRID extraction: only EPSG codes are supported in v1.
fn parse_srid(crs: &str) -> Result<i32, SourceError> {
    let rest = crs
        .strip_prefix("EPSG:")
        .ok_or_else(|| SourceError::Backend(format!("unsupported CRS: {crs}")))?;
    rest.parse::<i32>()
        .map_err(|_| SourceError::Backend(format!("unsupported CRS: {crs}")))
}

/// Compose `SELECT id, ST_AsBinary(geom), attrs... FROM s.t WHERE ST_Intersects(...) [AND filter]`.
pub(crate) fn build_query(
    binding: &SourceBinding,
    bbox: Bbox,
    srid: i32,
    filter: Option<&Expr>,
) -> Result<(String, Vec<SqlParam>), SourceError> {
    let id_q = quote_ident(&binding.id_column)?;
    let geom_q = quote_ident(&binding.geometry_column)?;
    let schema_q = quote_ident(&binding.from_schema)?;
    let table_q = quote_ident(&binding.from_table)?;

    let mut select = format!("{id_q}, ST_AsBinary({geom_q}) AS geom");
    for a in &binding.attributes {
        let q = quote_ident(a)?;
        select.push_str(", ");
        select.push_str(&q);
    }

    // spatial params land on $1..$5 first
    let mut params: Vec<SqlParam> = vec![
        SqlParam::Float(bbox.min_x),
        SqlParam::Float(bbox.min_y),
        SqlParam::Float(bbox.max_x),
        SqlParam::Float(bbox.max_y),
        SqlParam::Int(srid as i64),
    ];

    let mut sql = format!(
        "SELECT {select} FROM {schema_q}.{table_q} WHERE ST_Intersects({geom_q}, ST_MakeEnvelope($1, $2, $3, $4, $5))"
    );

    if let Some(expr) = filter {
        let (frag, fparams) = lower_to_sql(expr, binding)?;
        // renumber filter params: $N in `frag` becomes $(N + offset)
        let offset = params.len();
        let renumbered = renumber_params(&frag, offset);
        sql.push_str(" AND (");
        sql.push_str(&renumbered);
        sql.push(')');
        params.extend(fparams);
    }

    Ok((sql, params))
}

/// shifts placeholder `$N` tokens by `offset` positions.
fn renumber_params(s: &str, offset: usize) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                let quote = bytes[i];
                let start = i;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == quote {
                        i += 1;
                        if i < bytes.len() && bytes[i] == quote {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                out.push_str(&s[start..i]);
            }
            b'$' => {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j > i + 1 {
                    let n: usize = s[i + 1..j].parse().unwrap_or(0);
                    out.push_str(&format!("${}", n + offset));
                    i = j;
                } else {
                    out.push('$');
                    i += 1;
                }
            }
            _ => {
                let ch = s[i..].chars().next().unwrap_or_default();
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

fn decode_row(row: &tokio_postgres::Row, binding: &SourceBinding) -> Result<RowBytes, SourceError> {
    // col 0 = id, col 1 = wkb geom, col 2.. = attrs in binding order
    let id_signed: i64 = read_int(row, 0)?;
    if id_signed < 0 {
        return Err(SourceError::Backend(format!(
            "negative feature id rejected: {id_signed}"
        )));
    }
    #[allow(clippy::cast_sign_loss)]
    let feature_id = id_signed as u64;

    let wkb: Vec<u8> = row
        .try_get::<_, Vec<u8>>(1)
        .map_err(|e| SourceError::Backend(format!("decode geom: {e}")))?;
    let geometry = Bytes::from(wkb);

    let mut attributes = Vec::with_capacity(binding.attributes.len());
    for (i, name) in binding.attributes.iter().enumerate() {
        let col_idx = 2 + i;
        let v = decode_attr(row, col_idx)?;
        attributes.push((name.clone(), v));
    }

    Ok(RowBytes {
        feature_id,
        geometry,
        attributes,
    })
}

/// read a signed integer id column accepting INT2/INT4/INT8.
fn read_int(row: &tokio_postgres::Row, idx: usize) -> Result<i64, SourceError> {
    let col_ty = row.columns()[idx].type_();
    let v = match *col_ty {
        Type::INT2 => i64::from(
            row.try_get::<_, i16>(idx)
                .map_err(|e| SourceError::Backend(format!("decode i2: {e}")))?,
        ),
        Type::INT4 => i64::from(
            row.try_get::<_, i32>(idx)
                .map_err(|e| SourceError::Backend(format!("decode i4: {e}")))?,
        ),
        Type::INT8 => row
            .try_get::<_, i64>(idx)
            .map_err(|e| SourceError::Backend(format!("decode i8: {e}")))?,
        ref other => {
            return Err(SourceError::Backend(format!(
                "unsupported pg type for id column: {other}"
            )));
        }
    };
    Ok(v)
}

fn decode_attr(row: &tokio_postgres::Row, idx: usize) -> Result<AttrValue, SourceError> {
    let col_ty = row.columns()[idx].type_();
    let v = match *col_ty {
        Type::BOOL => row
            .try_get::<_, Option<bool>>(idx)
            .map_err(map_decode_err("bool"))?
            .map_or(AttrValue::Null, AttrValue::Bool),
        Type::INT2 => row
            .try_get::<_, Option<i16>>(idx)
            .map_err(map_decode_err("int2"))?
            .map_or(AttrValue::Null, |x| AttrValue::Int(i64::from(x))),
        Type::INT4 => row
            .try_get::<_, Option<i32>>(idx)
            .map_err(map_decode_err("int4"))?
            .map_or(AttrValue::Null, |x| AttrValue::Int(i64::from(x))),
        Type::INT8 => row
            .try_get::<_, Option<i64>>(idx)
            .map_err(map_decode_err("int8"))?
            .map_or(AttrValue::Null, AttrValue::Int),
        Type::FLOAT4 => row
            .try_get::<_, Option<f32>>(idx)
            .map_err(map_decode_err("float4"))?
            .map_or(AttrValue::Null, |x| AttrValue::Float(f64::from(x))),
        Type::FLOAT8 => row
            .try_get::<_, Option<f64>>(idx)
            .map_err(map_decode_err("float8"))?
            .map_or(AttrValue::Null, AttrValue::Float),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR => row
            .try_get::<_, Option<String>>(idx)
            .map_err(map_decode_err("text"))?
            .map_or(AttrValue::Null, AttrValue::String),
        ref other => {
            return Err(SourceError::Backend(format!("unsupported pg type: {other}")));
        }
    };
    Ok(v)
}

fn map_decode_err(label: &'static str) -> impl Fn(tokio_postgres::Error) -> SourceError {
    move |e| SourceError::Backend(format!("decode {label}: {e}"))
}

// `SqlParam` -> `ToSql` so it can drive both `client.query` and unit tests.
impl ToSql for SqlParam {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut bytes::BytesMut,
    ) -> Result<tokio_postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>>
    where
        Self: Sized,
    {
        match self {
            SqlParam::Null => Ok(tokio_postgres::types::IsNull::Yes),
            SqlParam::Bool(b) => match *ty {
                Type::BOOL => b.to_sql(ty, out),
                _ => Err(format!("cannot bind bool to {ty}").into()),
            },
            SqlParam::Int(i) => match *ty {
                // postgres binds parameters by exact wire type; an i64 sent
                // for an INT4 slot trips "incorrect binary data format". narrow
                // when we know the target is smaller. lossy conversions are
                // rejected up front rather than silently truncating.
                Type::INT2 => i16::try_from(*i)
                    .map_err(|_| -> Box<dyn std::error::Error + Sync + Send> {
                        format!("int {i} out of range for INT2").into()
                    })?
                    .to_sql(ty, out),
                Type::INT4 => i32::try_from(*i)
                    .map_err(|_| -> Box<dyn std::error::Error + Sync + Send> {
                        format!("int {i} out of range for INT4").into()
                    })?
                    .to_sql(ty, out),
                Type::INT8 => i.to_sql(ty, out),
                _ => Err(format!("cannot bind integer to {ty}").into()),
            },
            SqlParam::Float(f) => match *ty {
                Type::FLOAT4 => (*f as f32).to_sql(ty, out),
                Type::FLOAT8 => f.to_sql(ty, out),
                _ => Err(format!("cannot bind float to {ty}").into()),
            },
            SqlParam::Text(s) => match *ty {
                Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => s.to_sql(ty, out),
                _ => Err(format!("cannot bind text to {ty}").into()),
            },
        }
    }

    fn accepts(_ty: &Type) -> bool {
        // postgres validates per-call via to_sql_checked; accept-all here lets
        // the variant pick an appropriate target type at bind time.
        true
    }

    tokio_postgres::types::to_sql_checked!();
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use mars_expr::parse;
    use mars_source::SourceCollectionId;
    use mars_types::CrsCode;

    fn b() -> SourceBinding {
        SourceBinding::new(
            SourceCollectionId::new("c"),
            "public",
            "t",
            "geom",
            "gid",
            vec!["name".into(), "kind".into()],
            CrsCode::new("EPSG:25832"),
        )
        .unwrap()
    }

    #[test]
    fn srid_parsing() {
        assert_eq!(parse_srid("EPSG:25832").unwrap(), 25832);
        assert!(parse_srid("CRS84").is_err());
        assert!(parse_srid("EPSG:abc").is_err());
    }

    #[test]
    fn query_no_filter() {
        let bbox = Bbox::new(0.0, 0.0, 100.0, 100.0);
        let (sql, params) = build_query(&b(), bbox, 25832, None).unwrap();
        assert!(sql.contains("ST_AsBinary(\"geom\")"));
        assert!(sql.contains("FROM \"public\".\"t\""));
        assert!(sql.contains("ST_MakeEnvelope($1, $2, $3, $4, $5)"));
        assert_eq!(params.len(), 5);
    }

    #[test]
    fn query_with_filter_renumbers() {
        let bbox = Bbox::new(0.0, 0.0, 100.0, 100.0);
        let e = parse("name = 'x' AND kind = 1").unwrap();
        let (sql, params) = build_query(&b(), bbox, 25832, Some(&e)).unwrap();
        assert!(sql.contains("AND (\"name\" = $6 AND \"kind\" = $7)"));
        assert_eq!(params.len(), 7);
    }

    #[test]
    fn id_column_only_attrs() {
        let binding = SourceBinding::new(
            SourceCollectionId::new("c"),
            "public",
            "t",
            "geom",
            "gid",
            vec![],
            CrsCode::new("EPSG:25832"),
        )
        .unwrap();
        let e = parse("gid > 0").unwrap();
        let bbox = Bbox::new(0.0, 0.0, 1.0, 1.0);
        let (sql, params) = build_query(&binding, bbox, 25832, Some(&e)).unwrap();
        assert!(sql.contains("AND (\"gid\" > $6)"));
        assert_eq!(params.len(), 6);
    }

    #[test]
    fn query_renumbering_ignores_dollars_inside_quoted_identifiers() {
        let binding = SourceBinding::new(
            SourceCollectionId::new("c"),
            "public",
            "t",
            "geom",
            "gid",
            vec!["cost$1".into()],
            CrsCode::new("EPSG:25832"),
        )
        .unwrap();
        let e = Expr::Cmp {
            op: mars_expr::CmpOp::Eq,
            lhs: Box::new(Expr::Ident("cost$1".into())),
            rhs: Box::new(Expr::Literal(mars_expr::Literal::Int(10))),
        };

        let bbox = Bbox::new(0.0, 0.0, 1.0, 1.0);
        let (sql, params) = build_query(&binding, bbox, 25832, Some(&e)).unwrap();

        assert!(sql.contains("AND (\"cost$1\" = $6)"), "{sql}");
        assert_eq!(params.len(), 6);
    }
}
