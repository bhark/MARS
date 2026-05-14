//! Snapshot-isolated compile session.
//!
//! Holds one pooled connection in a `REPEATABLE READ` transaction so the
//! pass-1 geometry summary scan and pass-2 row hydration scan that drive
//! the unified compile pipeline see the same row set. Adapter side of the
//! `mars_source::CompileSession` port.
//!
//! Pass-1 (`stream_geometry_summary`) and pass-2 (`stream_rows`) both emit
//! `(tableoid, ctid)` as the snapshot-stable row identity packed into a
//! [`SourceRowKey`]. Both columns are read-only system columns supported by
//! every heap-backed relation; the page planner uses the key as the terminal
//! sort tier after `(hilbert_key, feature_id)` and pass-2 buckets streamed
//! rows into planned pages by joining on it. A single sequential scan avoids
//! per-page `WHERE id = ANY($1)` round-trips, whose heap-walk cost dominated
//! compile time on large bindings.
//!
//! View-shaped (`sql:`) bindings carry a parenthesised inline `SELECT` as
//! their `from` locator. `tableoid` / `ctid` are only valid on heap-backed
//! relations - on a derived table the outer query cannot reference them.
//! The session materialises such bindings into a per-session `TEMP TABLE …
//! ON COMMIT DROP` so pass-1 / pass-2 run against a real heap with the
//! standard identity columns; the table dies with the session's transaction
//! (commit drops; rollback never persisted it). Cost is one extra
//! server-side scan to populate the temp table; the snapshot pipeline was
//! going to enumerate every row anyway.

use async_trait::async_trait;
use deadpool_postgres::Object;
use futures_core::stream::BoxStream;
use futures_util::StreamExt;
use mars_source::{CompileSession, RowBytes, RowSummary, SourceBinding, SourceError, SourceRowKey};

use crate::SqlParam;
use crate::fetch::{append_binding_filter, decode_row_pub};
use crate::quote::{quote_ident, render_from_target};

/// Per-session temp table that materialises a `sql:` binding's inline SELECT
/// so pass-1 / pass-2 can reference `tableoid` / `ctid`. Lives in the
/// session-private `pg_temp` schema; ON COMMIT DROP makes the lifecycle a
/// no-op for the caller.
const TEMP_TABLE_NAME: &str = "_mars_compile_src";

/// One compile-time session against a single binding. Owns a pooled
/// connection in `REPEATABLE READ` until the caller invokes `commit` or
/// `rollback`. `Drop` performs no I/O; the pool's `pre_recycle` hook
/// rolls back any leftover transaction before the connection is reused.
pub(crate) struct PgCompileSession {
    object: Option<Object>,
    binding: SourceBinding,
    summary_sql: String,
    summary_params: Vec<SqlParam>,
    full_sql: String,
    full_params: Vec<SqlParam>,
    closed: bool,
}

impl PgCompileSession {
    pub(crate) async fn open(pool: deadpool_postgres::Pool, binding: SourceBinding) -> Result<Self, SourceError> {
        // sql: binding -- the locator is `(SELECT …)`. point queries at a
        // per-session temp table populated from that SELECT; pass-1 / pass-2
        // then see a real heap with tableoid / ctid available.
        let sql_binding_select = binding.from.starts_with('(').then(|| binding.from.clone());
        let effective_binding = if sql_binding_select.is_some() {
            SourceBinding {
                from: format!("pg_temp.{TEMP_TABLE_NAME}"),
                ..binding.clone()
            }
        } else {
            binding.clone()
        };

        let (summary_sql, summary_params) = build_summary_query(&effective_binding)?;
        let (full_sql, full_params) = build_full_table_query(&effective_binding)?;

        let object = pool.get().await.map_err(|e| SourceError::backend("pool checkout", e))?;
        // snapshot isolation across pass-1 + pass-2 scans. pass-2 is a single
        // unbounded streaming scan whose wall-clock equals the consumer's
        // processing time for the whole binding; the pool-level
        // statement_timeout (sized for ad-hoc / replication paths) is the
        // wrong instrument for it. SET LOCAL clears the cap for this txn only
        // and self-resets at COMMIT/ROLLBACK -- no leak to the next checkout.
        // max_parallel_workers_per_gather=0 + synchronize_seqscans=off pin the
        // pass-2 heap scan to a single worker emitting rows in (tableoid,
        // block, offset) ascending order, which matches the BE row_key byte
        // order so the cursor walk in mars-compiler stays monotonic.
        object
            .batch_execute(
                "BEGIN ISOLATION LEVEL REPEATABLE READ; \
                 SET LOCAL statement_timeout = 0; \
                 SET LOCAL max_parallel_workers_per_gather = 0; \
                 SET LOCAL synchronize_seqscans = off",
            )
            .await
            .map_err(|e| SourceError::backend("begin compile session", e))?;

        if let Some(select) = sql_binding_select {
            // ON COMMIT DROP ties the temp table's lifetime to this
            // transaction; ROLLBACK never persists the relation in the first
            // place. select is already a parenthesised SELECT so it splices
            // directly into CREATE TABLE AS.
            let stmt = format!("CREATE TEMP TABLE {TEMP_TABLE_NAME} ON COMMIT DROP AS {select}");
            object
                .batch_execute(&stmt)
                .await
                .map_err(|e| SourceError::backend("materialise sql binding", e))?;
        }

        Ok(Self {
            object: Some(object),
            binding: effective_binding,
            summary_sql,
            summary_params,
            full_sql,
            full_params,
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
    async fn stream_geometry_summary<'a>(
        &'a mut self,
    ) -> Result<BoxStream<'a, Result<RowSummary, SourceError>>, SourceError> {
        let object = self.client()?;
        let row_stream = object
            .query_raw(&self.summary_sql, self.summary_params.iter())
            .await
            .map_err(|e| SourceError::backend("query_raw summary", e))?;
        let mapped = row_stream.map(|item| match item {
            Ok(row) => decode_summary(&row),
            Err(e) => Err(SourceError::backend("row stream summary", e)),
        });
        Ok(Box::pin(mapped))
    }

    async fn stream_rows<'a>(&'a mut self) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        let object = self.client()?;
        let row_stream = object
            .query_raw(&self.full_sql, self.full_params.iter())
            .await
            .map_err(|e| SourceError::backend("query_raw full_table (session)", e))?;
        let binding = self.binding.clone();
        let mapped = row_stream.map(move |item| match item {
            Ok(row) => decode_compile_row(&row, &binding),
            Err(e) => Err(SourceError::backend("row stream full_table", e)),
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
    let tableoid: u32 = row
        .try_get::<_, u32>(1)
        .map_err(|e| SourceError::backend("decode_summary tableoid", e))?;
    let ctid_text: &str = row
        .try_get::<_, &str>(2)
        .map_err(|e| SourceError::backend("decode_summary ctid", e))?;
    let xmin: f32 = row
        .try_get::<_, f32>(3)
        .map_err(|e| SourceError::backend("decode_summary xmin", e))?;
    let ymin: f32 = row
        .try_get::<_, f32>(4)
        .map_err(|e| SourceError::backend("decode_summary ymin", e))?;
    let xmax: f32 = row
        .try_get::<_, f32>(5)
        .map_err(|e| SourceError::backend("decode_summary xmax", e))?;
    let ymax: f32 = row
        .try_get::<_, f32>(6)
        .map_err(|e| SourceError::backend("decode_summary ymax", e))?;
    let len: i32 = row
        .try_get::<_, i32>(7)
        .map_err(|e| SourceError::backend("decode_summary len", e))?;
    let geom_byte_length = u32::try_from(len.max(0))
        .map_err(|_| SourceError::backend_msg("decode_summary", "octet_length out of u32 range"))?;
    let (block, offset) = parse_ctid(ctid_text)?;
    Ok(RowSummary {
        feature_id: id,
        bbox: [xmin, ymin, xmax, ymax],
        geom_byte_length,
        row_key: pack_row_key(tableoid, block, offset),
    })
}

/// Parse postgres' textual ctid `(block,offset)` into its numeric parts.
/// Block is `BlockNumber` (u32); offset is `OffsetNumber` (u16); both are
/// non-negative.
fn parse_ctid(s: &str) -> Result<(u32, u16), SourceError> {
    let inner = s
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(|| SourceError::backend_msg("decode_summary ctid", format!("malformed ctid {s:?}")))?;
    let (block_s, offset_s) = inner
        .split_once(',')
        .ok_or_else(|| SourceError::backend_msg("decode_summary ctid", format!("ctid missing comma {s:?}")))?;
    let block: u32 = block_s
        .parse()
        .map_err(|e: std::num::ParseIntError| SourceError::backend("decode_summary ctid block", e))?;
    let offset: u16 = offset_s
        .parse()
        .map_err(|e: std::num::ParseIntError| SourceError::backend("decode_summary ctid offset", e))?;
    Ok((block, offset))
}

/// Pack `(tableoid, block, offset)` into the 16-byte `SourceRowKey`. Layout:
/// `tableoid (u32 BE) || block (u32 BE) || offset (u16 BE) || pad (6B 0)`.
/// 10 useful bytes; padding reserved for future routing metadata. BE so
/// lexicographic byte order on the key equals numeric `(tableoid, block,
/// offset)` order, matching the single-worker heap-scan emission order
/// pinned by the pass-2 compile session.
fn pack_row_key(tableoid: u32, block: u32, offset: u16) -> SourceRowKey {
    let mut k = [0u8; 16];
    k[0..4].copy_from_slice(&tableoid.to_be_bytes());
    k[4..8].copy_from_slice(&block.to_be_bytes());
    k[8..10].copy_from_slice(&offset.to_be_bytes());
    SourceRowKey::from_bytes(k)
}

/// Pass-1 SQL: `SELECT id, tableoid, ctid, ST_XMin(geom), ST_YMin, ST_XMax,
/// ST_YMax, octet_length(ST_AsBinary(geom)) FROM s.t WHERE geom IS NOT NULL`.
/// The combined `(tableoid, ctid)` pair is the snapshot-stable row identity
/// used as the page planner's terminal sort tier.
///
/// rows with NULL geometry are filtered at SQL level: ST_XMin/ST_AsBinary
/// return NULL on NULL geom, which the non-Option decoders cannot represent.
/// the same predicate runs in pass-2 so the two scans stay row-set aligned
/// under the shared snapshot.
fn build_summary_query(binding: &SourceBinding) -> Result<(String, Vec<SqlParam>), SourceError> {
    let from_q = render_from_target(&binding.from)?;
    let id_q = quote_ident(&binding.id_field)?;
    let geom_q = quote_ident(&binding.geometry_field)?;
    let mut sql = format!(
        "SELECT {id_q}::int8, \
                tableoid::oid, \
                ctid::text, \
                ST_XMin({geom_q})::float4, \
                ST_YMin({geom_q})::float4, \
                ST_XMax({geom_q})::float4, \
                ST_YMax({geom_q})::float4, \
                octet_length(ST_AsBinary({geom_q}))::int4 \
         FROM {from_q} \
         WHERE {geom_q} IS NOT NULL"
    );
    let mut params: Vec<SqlParam> = Vec::new();
    append_binding_filter(&mut sql, &mut params, binding, 0)?;
    Ok((sql, params))
}

/// Pass-2 SQL: `SELECT id, ST_AsBinary(geom), attrs..., tableoid::oid,
/// ctid::text FROM s.t WHERE geom IS NOT NULL`. Single sequential scan in
/// pg-table order; pass-2's caller buckets rows into the planned pages by
/// joining on [`SourceRowKey`]. No `ORDER BY` -- avoids the per-page heap-walk
/// that the prior `id = ANY($1)` pattern degenerated to on non-clustered
/// tables. The NULL-geom predicate mirrors pass-1 so the two scans see the
/// same row set under the shared snapshot.
fn build_full_table_query(binding: &SourceBinding) -> Result<(String, Vec<SqlParam>), SourceError> {
    let from_q = render_from_target(&binding.from)?;
    let id_q = quote_ident(&binding.id_field)?;
    let geom_q = quote_ident(&binding.geometry_field)?;

    let mut select = format!("{id_q}, ST_AsBinary({geom_q}) AS geom");
    for a in &binding.attributes {
        let q = quote_ident(a)?;
        select.push_str(", ");
        select.push_str(&q);
    }
    // tableoid + ctid land at fixed trailing offsets so decode_compile_row
    // can locate them without re-deriving the attribute count.
    select.push_str(", tableoid::oid, ctid::text");
    let mut sql = format!("SELECT {select} FROM {from_q} WHERE {geom_q} IS NOT NULL");
    let mut params: Vec<SqlParam> = Vec::new();
    append_binding_filter(&mut sql, &mut params, binding, 0)?;
    Ok((sql, params))
}

/// Decode a pass-2 row produced by `build_full_table_query`. The first
/// `2 + binding.attributes.len()` columns are the standard `[id, geom,
/// attrs...]` shape that `decode_row_pub` already understands; tableoid +
/// ctid sit at the trailing offsets and become the [`SourceRowKey`].
fn decode_compile_row(row: &tokio_postgres::Row, binding: &SourceBinding) -> Result<RowBytes, SourceError> {
    let mut decoded = decode_row_pub(row, binding)?;
    let key_offset = 2 + binding.attributes.len();
    let tableoid: u32 = row
        .try_get::<_, u32>(key_offset)
        .map_err(|e| SourceError::backend("decode_compile_row tableoid", e))?;
    let ctid_text: &str = row
        .try_get::<_, &str>(key_offset + 1)
        .map_err(|e| SourceError::backend("decode_compile_row ctid", e))?;
    let (block, offset) = parse_ctid(ctid_text)?;
    decoded.row_key = pack_row_key(tableoid, block, offset);
    Ok(decoded)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn summary_sql_is_well_formed() {
        let b = SourceBinding::new(
            mars_source::SourceCollectionId::new("c"),
            "public.t",
            "geom",
            "gid",
            vec![],
            mars_types::CrsCode::new("EPSG:25832"),
        )
        .unwrap();
        let (sql, params) = build_summary_query(&b).unwrap();
        assert!(sql.contains("ST_XMin(\"geom\")::float4"));
        assert!(sql.contains("octet_length(ST_AsBinary(\"geom\"))::int4"));
        assert!(sql.contains("tableoid::oid"));
        assert!(sql.contains("ctid::text"));
        assert!(!sql.contains("md5"));
        assert!(sql.contains("FROM \"public\".\"t\""));
        assert!(sql.contains("WHERE \"geom\" IS NOT NULL"));
        assert!(params.is_empty());
    }

    #[test]
    fn full_table_sql_is_well_formed() {
        let b = SourceBinding::new(
            mars_source::SourceCollectionId::new("c"),
            "public.t",
            "geom",
            "gid",
            vec!["name".into(), "kind".into()],
            mars_types::CrsCode::new("EPSG:25832"),
        )
        .unwrap();
        let (sql, params) = build_full_table_query(&b).unwrap();
        assert_eq!(
            sql,
            "SELECT \"gid\", ST_AsBinary(\"geom\") AS geom, \"name\", \"kind\", tableoid::oid, ctid::text FROM \"public\".\"t\" WHERE \"geom\" IS NOT NULL"
        );
        assert!(!sql.contains("ORDER BY"));
        assert!(params.is_empty());
    }

    #[test]
    fn parse_ctid_round_trips() {
        let (b, o) = super::parse_ctid("(0,1)").unwrap();
        assert_eq!((b, o), (0, 1));
        let (b, o) = super::parse_ctid("(4294967295,65535)").unwrap();
        assert_eq!((b, o), (u32::MAX, u16::MAX));
    }

    #[test]
    fn parse_ctid_rejects_garbage() {
        assert!(super::parse_ctid("0,1").is_err());
        assert!(super::parse_ctid("(0)").is_err());
        assert!(super::parse_ctid("(x,y)").is_err());
    }

    #[test]
    fn pack_row_key_layout() {
        let k = super::pack_row_key(0x0011_2233, 0xaabb_ccdd, 0xeeff);
        let b = k.as_bytes();
        assert_eq!(&b[0..4], &0x0011_2233u32.to_be_bytes());
        assert_eq!(&b[4..8], &0xaabb_ccddu32.to_be_bytes());
        assert_eq!(&b[8..10], &0xeeffu16.to_be_bytes());
        assert_eq!(&b[10..], &[0u8; 6]);
    }

    #[test]
    fn pack_row_key_lex_order_matches_numeric_order() {
        let lo = super::pack_row_key(1, 1, 1);
        let mid = super::pack_row_key(1, 1, 2);
        let hi = super::pack_row_key(1, 2, 0);
        let top = super::pack_row_key(2, 0, 0);
        assert!(lo.as_bytes() < mid.as_bytes());
        assert!(mid.as_bytes() < hi.as_bytes());
        assert!(hi.as_bytes() < top.as_bytes());
    }
}
