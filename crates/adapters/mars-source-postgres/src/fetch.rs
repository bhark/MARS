//! Per-call SQL builder.
//!
//! Phase B note: cell-keyed `fetch_cell` / `fetch_cells` were retired with
//! the v3 substrate cut. Phase C reintroduces page-keyed entry points
//! (`fetch_full_table_streaming` for bootstrap, `fetch_by_feature_ids` for
//! incremental rebuild) on top of the same SQL builder + row decoder.

#![allow(dead_code)]

use bytes::Bytes;
use deadpool_postgres::Pool;
use futures_core::stream::BoxStream;
use futures_util::StreamExt;
use mars_expr::Expr;
use mars_source::{AttrValue, RowBytes, SourceBinding, SourceError, SourceRowKey};
use mars_types::Bbox;
use tokio_postgres::types::{ToSql, Type};

use crate::SqlParam;
use crate::lower::lower_to_sql;
use crate::quote::quote_ident;

const PG_ID_BATCH: usize = 16_384;

/// Stream every row of `binding`'s table in pg-table order. The producer runs
/// on a spawned task so the returned stream owns nothing borrowed from the
/// pool checkout; back-pressure flows through a bounded mpsc channel.
pub(crate) async fn fetch_full_table_streaming(
    pool: Pool,
    binding: SourceBinding,
) -> Result<BoxStream<'static, Result<RowBytes, SourceError>>, SourceError> {
    // build the SQL up front so we surface bad identifiers as InvalidBinding /
    // backend errors before the producer task is spawned.
    let sql = build_full_table_query(&binding)?;

    // bounded channel so a slow consumer back-pressures the producer rather
    // than letting an unbounded queue grow during a 50M-row bootstrap.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<RowBytes, SourceError>>(64);

    tokio::spawn(async move {
        let send_err = |e: SourceError, tx: &tokio::sync::mpsc::Sender<_>| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(Err(e)).await;
            }
        };

        let client = match pool.get().await {
            Ok(c) => c,
            Err(e) => {
                send_err(SourceError::backend("pool checkout", e), &tx).await;
                return;
            }
        };
        // empty params slice; query_raw still needs the iterator type fixed.
        let no_params: [&(dyn ToSql + Sync); 0] = [];
        let row_stream = match client.query_raw(&sql, no_params).await {
            Ok(s) => s,
            Err(e) => {
                send_err(SourceError::backend("query_raw full_table", e), &tx).await;
                return;
            }
        };
        tokio::pin!(row_stream);
        while let Some(item) = row_stream.next().await {
            let decoded = match item {
                Ok(row) => decode_row(&row, &binding),
                Err(e) => Err(SourceError::backend("row stream", e)),
            };
            if tx.send(decoded).await.is_err() {
                break;
            }
        }
    });

    let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
    Ok(Box::pin(stream))
}

pub(crate) async fn fetch_by_feature_ids(
    pool: Pool,
    binding: SourceBinding,
    ids: Vec<i64>,
) -> Result<BoxStream<'static, Result<RowBytes, SourceError>>, SourceError> {
    let sql = build_feature_ids_query(&binding)?;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<RowBytes, SourceError>>(64);

    if ids.is_empty() {
        drop(tx);
        let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
        return Ok(Box::pin(stream));
    }

    tokio::spawn(async move {
        let send_err = |e: SourceError, tx: &tokio::sync::mpsc::Sender<_>| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(Err(e)).await;
            }
        };

        let client = match pool.get().await {
            Ok(c) => c,
            Err(e) => {
                send_err(SourceError::backend("pool checkout", e), &tx).await;
                return;
            }
        };

        for chunk in ids.chunks(PG_ID_BATCH) {
            let chunk_ids = chunk.to_vec();
            let row_stream = match client.query_raw(&sql, [(&chunk_ids) as &(dyn ToSql + Sync)]).await {
                Ok(s) => s,
                Err(e) => {
                    send_err(SourceError::backend("query_raw feature_ids", e), &tx).await;
                    return;
                }
            };
            tokio::pin!(row_stream);
            while let Some(item) = row_stream.next().await {
                let decoded = match item {
                    Ok(row) => decode_row(&row, &binding),
                    Err(e) => Err(SourceError::backend("row stream", e)),
                };
                if tx.send(decoded).await.is_err() {
                    return;
                }
            }
        }
    });

    let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
    Ok(Box::pin(stream))
}

/// stream every feature id from `binding`'s table; used by the periodic
/// reconciliation hook to compare the source id set against the
/// page-membership sidecar's id set.
pub(crate) async fn stream_feature_ids(
    pool: Pool,
    binding: SourceBinding,
) -> Result<BoxStream<'static, Result<i64, SourceError>>, SourceError> {
    let sql = build_feature_ids_only_query(&binding)?;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<i64, SourceError>>(64);

    tokio::spawn(async move {
        let send_err = |e: SourceError, tx: &tokio::sync::mpsc::Sender<_>| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(Err(e)).await;
            }
        };
        let client = match pool.get().await {
            Ok(c) => c,
            Err(e) => {
                send_err(SourceError::backend("pool checkout", e), &tx).await;
                return;
            }
        };
        let no_params: [&(dyn ToSql + Sync); 0] = [];
        let row_stream = match client.query_raw(&sql, no_params).await {
            Ok(s) => s,
            Err(e) => {
                send_err(SourceError::backend("query_raw stream_feature_ids", e), &tx).await;
                return;
            }
        };
        tokio::pin!(row_stream);
        while let Some(item) = row_stream.next().await {
            let decoded = match item {
                Ok(row) => match read_int(&row, 0) {
                    Ok(Some(id)) if id >= 0 => Ok(id),
                    Ok(Some(id)) => Err(SourceError::backend_msg(
                        "stream_feature_ids",
                        format!("negative feature id rejected: {id}"),
                    )),
                    Ok(None) => Err(SourceError::backend_msg(
                        "stream_feature_ids",
                        "feature id column is NULL",
                    )),
                    Err(e) => Err(e),
                },
                Err(e) => Err(SourceError::backend("row stream", e)),
            };
            if tx.send(decoded).await.is_err() {
                break;
            }
        }
    });

    let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
    Ok(Box::pin(stream))
}

fn build_feature_ids_only_query(binding: &SourceBinding) -> Result<String, SourceError> {
    let id_q = quote_ident(&binding.id_column)?;
    let schema_q = quote_ident(&binding.from_schema)?;
    let table_q = quote_ident(&binding.from_table)?;
    Ok(format!("SELECT {id_q} FROM {schema_q}.{table_q}"))
}

/// `SELECT id, ST_AsBinary(geom), attrs... FROM s.t` -- no spatial filter, no
/// ORDER BY. Used by snapshot bootstrap.
fn build_full_table_query(binding: &SourceBinding) -> Result<String, SourceError> {
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
    Ok(format!("SELECT {select} FROM {schema_q}.{table_q}"))
}

fn build_feature_ids_query(binding: &SourceBinding) -> Result<String, SourceError> {
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
    Ok(format!(
        "SELECT {select} FROM {schema_q}.{table_q} WHERE {id_q} = ANY($1::bigint[])"
    ))
}

/// SRID extraction: only EPSG codes are supported in v1.
fn parse_srid(crs: &str) -> Result<i32, SourceError> {
    let rest = crs
        .strip_prefix("EPSG:")
        .ok_or_else(|| SourceError::backend_msg("parse_srid", format!("unsupported CRS: {crs}")))?;
    rest.parse::<i32>()
        .map_err(|_| SourceError::backend_msg("parse_srid", format!("unsupported CRS: {crs}")))
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
        // lowerer emits placeholders starting at `params.len() + 1`
        let start = params.len() + 1;
        let (frag, fparams) = lower_to_sql(expr, binding, start)?;
        sql.push_str(" AND (");
        sql.push_str(&frag);
        sql.push(')');
        params.extend(fparams);
    }

    Ok((sql, params))
}

/// pub-crate alias so the compile-session module can reuse the row decoder
/// without re-deriving it.
pub(crate) fn decode_row_pub(row: &tokio_postgres::Row, binding: &SourceBinding) -> Result<RowBytes, SourceError> {
    decode_row(row, binding)
}

fn decode_row(row: &tokio_postgres::Row, binding: &SourceBinding) -> Result<RowBytes, SourceError> {
    // col 0 = id, col 1 = wkb geom, col 2.. = attrs in binding order. NULL ids
    // would silently coerce to 0 in some pg type paths; reject them up front so
    // a row with no id can never collide with a real feature_id of zero.
    let id_signed: i64 =
        read_int(row, 0)?.ok_or_else(|| SourceError::backend_msg("decode_row", "feature id column is NULL"))?;
    if id_signed < 0 {
        return Err(SourceError::backend_msg(
            "decode_row",
            format!("negative feature id rejected: {id_signed}"),
        ));
    }
    #[allow(clippy::cast_sign_loss)]
    let feature_id = id_signed as u64;

    let wkb: Vec<u8> = row
        .try_get::<_, Vec<u8>>(1)
        .map_err(|e| SourceError::backend("decode_geom", e))?;
    let geometry = Bytes::from(wkb);

    let mut attributes = Vec::with_capacity(binding.attributes.len());
    for (i, name) in binding.attributes.iter().enumerate() {
        let col_idx = 2 + i;
        let v = decode_attr(row, col_idx)?;
        attributes.push((name.clone(), v));
    }

    // stateless source path has no snapshot to anchor row identity.
    Ok(RowBytes {
        feature_id,
        geometry,
        attributes,
        row_key: SourceRowKey::ZERO,
    })
}

/// read a signed integer id column accepting INT2/INT4/INT8. returns Ok(None)
/// when the column is SQL NULL so the caller can decide whether NULL is valid;
/// for feature ids it is not.
fn read_int(row: &tokio_postgres::Row, idx: usize) -> Result<Option<i64>, SourceError> {
    let col_ty = row.columns()[idx].type_();
    let v = match *col_ty {
        Type::INT2 => row
            .try_get::<_, Option<i16>>(idx)
            .map_err(|e| SourceError::backend("decode_i2", e))?
            .map(i64::from),
        Type::INT4 => row
            .try_get::<_, Option<i32>>(idx)
            .map_err(|e| SourceError::backend("decode_i4", e))?
            .map(i64::from),
        Type::INT8 => row
            .try_get::<_, Option<i64>>(idx)
            .map_err(|e| SourceError::backend("decode_i8", e))?,
        ref other => {
            return Err(SourceError::backend_msg(
                "decode_row",
                format!("unsupported pg type for id column: {other}"),
            ));
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
            return Err(SourceError::backend_msg(
                "decode_attr",
                format!("unsupported pg type: {other}"),
            ));
        }
    };
    Ok(v)
}

fn map_decode_err(label: &'static str) -> impl Fn(tokio_postgres::Error) -> SourceError {
    move |e| SourceError::backend(label, e)
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
                Type::FLOAT4 => {
                    // mirror INT2/INT4 narrowing: refuse to silently truncate
                    // when the f64 cannot round-trip through f32.
                    let narrow = *f as f32;
                    if narrow as f64 != *f && !f.is_nan() {
                        return Err(format!("float {f} loses precision narrowing to FLOAT4").into());
                    }
                    narrow.to_sql(ty, out)
                }
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
    fn feature_ids_query_quotes_identifiers() {
        let sql = build_feature_ids_query(&b()).unwrap();
        assert_eq!(
            sql,
            "SELECT \"gid\", ST_AsBinary(\"geom\") AS geom, \"name\", \"kind\" FROM \"public\".\"t\" WHERE \"gid\" = ANY($1::bigint[])"
        );
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
    fn lowerer_emits_correct_placeholders_for_multi_segment_filter() {
        // multi-clause filter must be numbered contiguously, starting after
        // the spatial params ($1..$5).
        let bbox = Bbox::new(0.0, 0.0, 1.0, 1.0);
        let e = parse("name = 'a' AND kind IN (1, 2, 3) AND area >= 10").unwrap();
        let binding = SourceBinding::new(
            SourceCollectionId::new("c"),
            "public",
            "t",
            "geom",
            "gid",
            vec!["name".into(), "kind".into(), "area".into()],
            CrsCode::new("EPSG:25832"),
        )
        .unwrap();
        let (sql, params) = build_query(&binding, bbox, 25832, Some(&e)).unwrap();
        assert!(
            sql.contains("AND (\"name\" = $6 AND \"kind\" IN ($7, $8, $9) AND \"area\" >= $10)"),
            "{sql}"
        );
        assert_eq!(params.len(), 10);
    }
}
