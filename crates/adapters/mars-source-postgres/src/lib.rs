//! PostgreSQL adapter for `mars-source`.
//!
//! Two strategies behind the same `ChangeFeed` trait:
//! - `pgoutput` logical decoding (default; SPEC §8.2.1).
//! - Polling fallback under the `polling` feature (SPEC §8.2.2; second-class).
//!
//! This crate also owns the lowering of `mars-expr::Expr` to a parameterised
//! SQL `WHERE` clause. The lowering lives here, not in `mars-expr`, because
//! database vocabulary belongs in the database adapter and parameterisation
//! is enforceable in one place.

#![forbid(unsafe_code)]

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use mars_expr::Expr;
use mars_source::{ChangeEvent, ChangeFeed, RowBytes, Source, SourceBinding, SourceError};
use mars_types::Cell;

/// Connection / topology configuration. Filled in during Phase 1.
#[derive(Debug, Clone, Default)]
pub struct PgConfig {
    /// libpq DSN.
    pub dsn: String,
    /// Logical replication publication name.
    pub publication: String,
    /// Logical replication slot name.
    pub slot: String,
}

/// Phase-0 stub adapter. All methods return `NotImplemented` so the
/// composition root can wire it without a real database.
#[derive(Debug, Default)]
pub struct StubPg {
    _cfg: PgConfig,
}

impl StubPg {
    /// Construct a stub adapter from a (possibly-empty) config.
    #[must_use]
    pub fn new(cfg: PgConfig) -> Self {
        Self { _cfg: cfg }
    }
}

#[async_trait]
impl Source for StubPg {
    async fn fetch_cell(
        &self,
        _binding: &SourceBinding,
        _cell: &Cell,
        _filter: Option<&Expr>,
    ) -> Result<Vec<RowBytes>, SourceError> {
        // todo(SPEC §8.1) materialise rows for one cell from postgis
        Err(SourceError::NotImplemented {
            what: "mars-source-postgres::Source::fetch_cell",
        })
    }
}

#[async_trait]
impl ChangeFeed for StubPg {
    async fn subscribe(&self) -> Result<BoxStream<'static, Result<ChangeEvent, SourceError>>, SourceError> {
        // todo(SPEC §8.2.1) subscribe via pgoutput
        Err(SourceError::NotImplemented {
            what: "mars-source-postgres::ChangeFeed::subscribe",
        })
    }
}

/// Lowers a `mars-expr::Expr` to a parameterised SQL `WHERE` fragment plus
/// its bind parameters. Returns `NotImplemented` in Phase 0.
pub fn lower_to_sql(_expr: &Expr) -> Result<(String, Vec<SqlParam>), SourceError> {
    Err(SourceError::NotImplemented {
        what: "mars-source-postgres::lower_to_sql - Phase 1 (SPEC §5.6)",
    })
}

/// Bind parameter for a lowered SQL fragment.
#[derive(Debug, Clone)]
pub enum SqlParam {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use mars_source::SourceCollectionId;
    use mars_types::{CrsCode, ScaleBand};

    #[tokio::test]
    async fn stub_returns_not_implemented() {
        let s = StubPg::default();
        let binding = SourceBinding::new(
            SourceCollectionId::new("x"),
            "public",
            "x",
            "geom",
            "gid",
            vec![],
            CrsCode::new("EPSG:25832"),
        )
        .unwrap();
        let cell = Cell {
            band: ScaleBand::new("hi"),
            x: 0,
            y: 0,
        };
        let r = s.fetch_cell(&binding, &cell, None).await;
        assert!(matches!(r, Err(SourceError::NotImplemented { .. })));
    }
}
