//! Snapshot-isolated compile session.
//!
//! Holds one pooled connection in a `REPEATABLE READ` transaction so the
//! pass-1 geometry summary scan and pass-2 row hydration scan that drive
//! the unified compile pipeline see the same row set. Adapter side of the
//! `mars_source::CompileSession` port.
//!
//! Pass-1 embeds a `md5(ST_AsBinary(geom))`-derived u64 digest in
//! `RowSummary::geom_digest` server-side. md5 ships with every Postgres
//! install (no extension dependency); the digest is a page-planner sort
//! tiebreaker, not a security boundary, so md5's collision profile is
//! more than sufficient.

use async_trait::async_trait;
use deadpool_postgres::Object;
use futures_core::stream::BoxStream;
use futures_util::StreamExt;
use mars_source::{CompileSession, RowBytes, RowSummary, SourceBinding, SourceError};
use tokio_postgres::types::ToSql;

use crate::fetch::decode_row_pub;
use crate::quote::quote_ident;

/// One compile-time session against a single binding. Owns a pooled
/// connection in `REPEATABLE READ` until the caller invokes `commit` or
/// `rollback`. `Drop` performs no I/O; the pool's `pre_recycle` hook
/// rolls back any leftover transaction before the connection is reused.
pub(crate) struct PgCompileSession {
    object: Option<Object>,
    binding: SourceBinding,
    summary_sql: String,
    ids_sql: String,
    closed: bool,
}

impl PgCompileSession {
    pub(crate) async fn open(pool: deadpool_postgres::Pool, binding: SourceBinding) -> Result<Self, SourceError> {
        let summary_sql = build_summary_query(&binding)?;
        let ids_sql = build_feature_ids_query(&binding)?;

        let object = pool.get().await.map_err(|e| SourceError::backend("pool checkout", e))?;
        // snapshot isolation across pass-1 + pass-2 scans.
        object
            .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
            .await
            .map_err(|e| SourceError::backend("begin compile session", e))?;

        Ok(Self {
            object: Some(object),
            binding,
            summary_sql,
            ids_sql,
            closed: false,
        })
    }

    fn client(&self) -> Result<&Object, SourceError> {
        self.object
            .as_ref()
            .ok_or_else(|| SourceError::backend_msg("compile session", "session connection already taken"))
    }
}

#[async_trait]
impl CompileSession for PgCompileSession {
    async fn fetch_geometry_summary<'a>(
        &'a mut self,
    ) -> Result<BoxStream<'a, Result<RowSummary, SourceError>>, SourceError> {
        let object = self.client()?;
        let no_params: [&(dyn ToSql + Sync); 0] = [];
        let row_stream = object
            .query_raw(&self.summary_sql, no_params)
            .await
            .map_err(|e| SourceError::backend("query_raw summary", e))?;
        let mapped = row_stream.map(|item| match item {
            Ok(row) => decode_summary(&row),
            Err(e) => Err(SourceError::backend("row stream summary", e)),
        });
        Ok(Box::pin(mapped))
    }

    async fn fetch_by_feature_ids<'a>(
        &'a mut self,
        ids: &'a [i64],
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        if ids.is_empty() {
            return Ok(Box::pin(futures_util::stream::empty()));
        }
        let object = self.client()?;
        let chunk: Vec<i64> = ids.to_vec();
        let row_stream = object
            .query_raw(&self.ids_sql, [(&chunk) as &(dyn ToSql + Sync)])
            .await
            .map_err(|e| SourceError::backend("query_raw feature_ids (session)", e))?;
        let binding = self.binding.clone();
        let mapped = row_stream.map(move |item| match item {
            Ok(row) => decode_row_pub(&row, &binding),
            Err(e) => Err(SourceError::backend("row stream feature_ids", e)),
        });
        Ok(Box::pin(mapped))
    }

    async fn commit(mut self: Box<Self>) -> Result<(), SourceError> {
        if let Some(object) = self.object.take() {
            self.closed = true;
            object
                .batch_execute("COMMIT")
                .await
                .map_err(|e| SourceError::backend("commit compile session", e))?;
        }
        Ok(())
    }

    async fn rollback(mut self: Box<Self>) -> Result<(), SourceError> {
        if let Some(object) = self.object.take() {
            self.closed = true;
            object
                .batch_execute("ROLLBACK")
                .await
                .map_err(|e| SourceError::backend("rollback compile session", e))?;
        }
        Ok(())
    }
}

impl Drop for PgCompileSession {
    fn drop(&mut self) {
        // sync, no I/O. correctness comes from the pool's pre_recycle hook,
        // which rolls back any leftover transaction before next checkout.
        // a warn here surfaces missing explicit commit/rollback as a hygiene
        // signal without making drop runtime-dependent.
        if !self.closed {
            tracing::warn!(
                collection = %self.binding.collection.as_str(),
                "compile session dropped without commit/rollback; pool recycle will abort the transaction",
            );
        }
    }
}

fn decode_summary(row: &tokio_postgres::Row) -> Result<RowSummary, SourceError> {
    let id: i64 = row
        .try_get::<_, i64>(0)
        .map_err(|e| SourceError::backend("decode_summary id", e))?;
    let xmin: f32 = row
        .try_get::<_, f32>(1)
        .map_err(|e| SourceError::backend("decode_summary xmin", e))?;
    let ymin: f32 = row
        .try_get::<_, f32>(2)
        .map_err(|e| SourceError::backend("decode_summary ymin", e))?;
    let xmax: f32 = row
        .try_get::<_, f32>(3)
        .map_err(|e| SourceError::backend("decode_summary xmax", e))?;
    let ymax: f32 = row
        .try_get::<_, f32>(4)
        .map_err(|e| SourceError::backend("decode_summary ymax", e))?;
    let len: i32 = row
        .try_get::<_, i32>(5)
        .map_err(|e| SourceError::backend("decode_summary len", e))?;
    let digest: i64 = row
        .try_get::<_, i64>(6)
        .map_err(|e| SourceError::backend("decode_summary digest", e))?;
    let geom_byte_length = u32::try_from(len.max(0))
        .map_err(|_| SourceError::backend_msg("decode_summary", "octet_length out of u32 range"))?;
    Ok(RowSummary {
        feature_id: id,
        bbox: [xmin, ymin, xmax, ymax],
        geom_byte_length,
        // Postgres bit(64)::bigint = signed BE bit-cast; reinterpret to u64.
        geom_digest: digest as u64,
    })
}

/// Pass-1 SQL: `SELECT id, ST_XMin(geom), ST_YMin, ST_XMax, ST_YMax,
/// octet_length(ST_AsBinary(geom)), md5_be64(ST_AsBinary(geom)) FROM s.t`.
fn build_summary_query(binding: &SourceBinding) -> Result<String, SourceError> {
    let id_q = quote_ident(&binding.id_column)?;
    let geom_q = quote_ident(&binding.geometry_column)?;
    let schema_q = quote_ident(&binding.from_schema)?;
    let table_q = quote_ident(&binding.from_table)?;
    Ok(format!(
        "SELECT {id_q}::int8, \
                ST_XMin({geom_q})::float4, \
                ST_YMin({geom_q})::float4, \
                ST_XMax({geom_q})::float4, \
                ST_YMax({geom_q})::float4, \
                octet_length(ST_AsBinary({geom_q}))::int4, \
                (('x' || substr(md5(ST_AsBinary({geom_q})), 1, 16))::bit(64))::bigint \
         FROM {schema_q}.{table_q}"
    ))
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn summary_sql_is_well_formed() {
        let b = SourceBinding::new(
            mars_source::SourceCollectionId::new("c"),
            "public",
            "t",
            "geom",
            "gid",
            vec![],
            mars_types::CrsCode::new("EPSG:25832"),
        )
        .unwrap();
        let sql = build_summary_query(&b).unwrap();
        assert!(sql.contains("ST_XMin(\"geom\")::float4"));
        assert!(sql.contains("octet_length(ST_AsBinary(\"geom\"))::int4"));
        assert!(sql.contains("substr(md5(ST_AsBinary(\"geom\")), 1, 16)"));
        assert!(sql.contains("FROM \"public\".\"t\""));
    }
}
