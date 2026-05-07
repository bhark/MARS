//! Port traits for source databases and change feeds.
//!
//! `Source` is the read interface used by the compiler to materialise
//! geometries and attributes for a `(source_collection, scale_band, cell)`.
//! `ChangeFeed` is the subscription interface that produces dirty-cell
//! events (SPEC §8.2). Both are runtime-agnostic - concrete adapters live
//! in `crates/adapters/mars-source-*`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod access;

use async_trait::async_trait;
use bytes::Bytes;
use mars_expr::Expr;
pub use mars_types::SourceCollectionId;
use mars_types::{Bbox, Cell, CrsCode};

pub use access::RowAttrs;

/// Errors produced by source adapters.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// The adapter does not implement this method yet. Used by Phase-0 stubs.
    #[error("not implemented: {what}")]
    NotImplemented {
        /// Human-readable name of the unimplemented operation.
        what: &'static str,
    },
    /// Connectivity, transport, or driver error.
    #[error("backend error: {0}")]
    Backend(String),
    /// The change feed slot was lost or fell too far behind. Recovery is via
    /// snapshot compile (SPEC §8.2.3).
    #[error("change feed gone; full snapshot required")]
    ChangeFeedGone,
    /// Invalid binding configuration.
    #[error("invalid binding: {0}")]
    InvalidBinding(String),
    /// A filter expression referenced an identifier outside the binding's
    /// allowlist (`binding.attributes ∪ {binding.id_column}`). SQL lowering
    /// refuses to inject unknown identifiers.
    #[error("unknown identifier: {name}")]
    UnknownIdent {
        /// Identifier that was not present in the allowlist.
        name: String,
    },
}

/// One change-feed event, lowered from a postgres logical-decoding message
/// or a polling diff.
#[derive(Debug, Clone)]
pub enum ChangeEvent {
    /// A row was inserted; new geometry occupies the listed cells.
    Insert {
        /// Logical name of the source table / collection.
        collection: SourceCollectionId,
        /// Cells touched by the new geometry.
        cells: Vec<Cell>,
    },
    /// A row was updated; cells = touched-by-old ∪ touched-by-new.
    Update {
        /// Logical name of the source table / collection.
        collection: SourceCollectionId,
        /// Cells touched by the old or new geometry.
        cells: Vec<Cell>,
    },
    /// A row was deleted; cells = touched-by-old.
    Delete {
        /// Logical name of the source table / collection.
        collection: SourceCollectionId,
        /// Cells touched by the old geometry.
        cells: Vec<Cell>,
    },
    /// The whole collection was truncated; every cell is dirty.
    Truncate {
        /// Logical name of the source table / collection.
        collection: SourceCollectionId,
    },
}

/// A committed batch of change events. Adapters MUST only emit a batch once
/// the upstream transaction has committed; the `source_version` cursor (e.g.
/// pgoutput LSN) lets the compiler advance manifest provenance atomically
/// per batch.
#[derive(Debug, Clone)]
pub struct ChangeBatch {
    /// Ordered events committed together.
    pub events: Vec<ChangeEvent>,
    /// Opaque source-side cursor identifying the committed position. `None`
    /// when the adapter has no notion of a cursor (polling fallback).
    pub source_version: Option<String>,
}

/// Source-side binding: maps a logical `SourceCollectionId` onto the physical
/// table, geometry/id columns, attribute projection, and CRS. Lives in
/// `mars-source` because every field is database-vocabulary, not domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceBinding {
    /// Logical collection name.
    pub collection: SourceCollectionId,
    /// Source schema (e.g. `public`).
    pub from_schema: String,
    /// Source table.
    pub from_table: String,
    /// Geometry column name.
    pub geometry_column: String,
    /// Stable feature-id column name.
    pub id_column: String,
    /// Ordered, deduplicated attribute projection.
    pub attributes: Vec<String>,
    /// Source CRS.
    pub crs: CrsCode,
}

impl SourceBinding {
    /// Construct after validating attribute uniqueness and non-empty
    /// schema/table/column names.
    pub fn new(
        collection: SourceCollectionId,
        from_schema: impl Into<String>,
        from_table: impl Into<String>,
        geometry_column: impl Into<String>,
        id_column: impl Into<String>,
        attributes: Vec<String>,
        crs: CrsCode,
    ) -> Result<Self, SourceError> {
        let from_schema = from_schema.into();
        let from_table = from_table.into();
        let geometry_column = geometry_column.into();
        let id_column = id_column.into();

        for (label, v) in [
            ("from_schema", &from_schema),
            ("from_table", &from_table),
            ("geometry_column", &geometry_column),
            ("id_column", &id_column),
        ] {
            if v.is_empty() {
                return Err(SourceError::InvalidBinding(format!("{label} is empty")));
            }
        }

        // dedup check, preserves order
        let mut seen = std::collections::HashSet::with_capacity(attributes.len());
        for a in &attributes {
            if a.is_empty() {
                return Err(SourceError::InvalidBinding("empty attribute name".into()));
            }
            if !seen.insert(a.as_str()) {
                return Err(SourceError::InvalidBinding(format!("duplicate attribute: {a}")));
            }
        }

        Ok(Self {
            collection,
            from_schema,
            from_table,
            geometry_column,
            id_column,
            attributes,
            crs,
        })
    }
}

/// Decoded attribute value. Mirrors `mars_expr::Literal` 1:1, lives in
/// `mars-source` because the source layer is where rows are materialised
/// before evaluator handoff.
#[derive(Debug, Clone, PartialEq)]
pub enum AttrValue {
    /// SQL NULL.
    Null,
    /// Boolean.
    Bool(bool),
    /// 64-bit signed integer.
    Int(i64),
    /// 64-bit float.
    Float(f64),
    /// UTF-8 string.
    String(String),
}

/// Read-side port: query geometry and attributes for a cell.
#[async_trait]
pub trait Source: Send + Sync + 'static {
    /// Materialise rows whose geometry intersects `cell`, optionally filtered
    /// by a `mars-expr` AST. The binding describes table mapping and
    /// attribute projection; `RowBytes.attributes` is returned in the same
    /// order as `binding.attributes`.
    ///
    /// `bbox` is the canonical-CRS extent of `cell`, precomputed by the
    /// caller (the compiler knows the band's cell-size and origin; the
    /// adapter does not need to). Adapters use it for the spatial predicate.
    async fn fetch_cell(
        &self,
        binding: &SourceBinding,
        cell: &Cell,
        bbox: Bbox,
        filter: Option<&Expr>,
    ) -> Result<Vec<RowBytes>, SourceError>;

    /// Materialise rows for a batch of `(cell, bbox)` pairs sharing one
    /// binding and filter. Adapters that can pipeline (e.g. postgres) override
    /// this to amortise per-call overhead; the default fans out to
    /// `fetch_cell`. The returned vec is ordered to mirror the input slice;
    /// each result carries its `Cell` back so the caller can route rows
    /// without relying on order.
    async fn fetch_cells(
        &self,
        binding: &SourceBinding,
        cells: &[(Cell, Bbox)],
        filter: Option<&Expr>,
    ) -> Result<Vec<(Cell, Vec<RowBytes>)>, SourceError> {
        let mut out = Vec::with_capacity(cells.len());
        for (cell, bbox) in cells {
            let rows = self.fetch_cell(binding, cell, *bbox, filter).await?;
            out.push((cell.clone(), rows));
        }
        Ok(out)
    }
}

/// Per-row record returned by `Source`. Geometry is opaque adapter-native
/// bytes (typically WKB); attributes are decoded into `(name, AttrValue)`
/// pairs ordered to match the binding's projection.
#[derive(Debug, Clone)]
pub struct RowBytes {
    /// Stable identifier of the row inside the source collection.
    pub feature_id: u64,
    /// Encoded geometry payload (opaque to the compiler).
    pub geometry: Bytes,
    /// Decoded attributes, ordered to mirror `SourceBinding.attributes`.
    pub attributes: Vec<(String, AttrValue)>,
}

/// Phase-0 stub adapters that satisfy the port traits with `NotImplemented`.
/// Lets bins and tests compose the surface without naming a real backend.
pub mod stub {
    use super::{ChangeFeed, ChangeSubscription, RowBytes, Source, SourceBinding, SourceError};
    use async_trait::async_trait;
    use mars_expr::Expr;
    use mars_types::{Bbox, Cell};

    /// `Source` + `ChangeFeed` impl that always returns `NotImplemented`.
    #[derive(Debug, Default)]
    pub struct NotImplementedSource;

    #[async_trait]
    impl Source for NotImplementedSource {
        async fn fetch_cell(
            &self,
            _binding: &SourceBinding,
            _cell: &Cell,
            _bbox: Bbox,
            _filter: Option<&Expr>,
        ) -> Result<Vec<RowBytes>, SourceError> {
            Err(SourceError::NotImplemented {
                what: "mars-source::stub::NotImplementedSource::fetch_cell",
            })
        }
    }

    #[async_trait]
    impl ChangeFeed for NotImplementedSource {
        async fn subscribe(&self) -> Result<Box<dyn ChangeSubscription>, SourceError> {
            Err(SourceError::NotImplemented {
                what: "mars-source::stub::NotImplementedSource::subscribe",
            })
        }
    }
}

/// Subscription-side port: a stream of committed [`ChangeBatch`]es with an
/// ack-aware cursor.
///
/// `subscribe` opens the subscription. The returned [`ChangeSubscription`] is
/// drained by the compiler one batch at a time; the compiler MUST call
/// [`ChangeSubscription::acknowledge`] only after a batch's effects have been
/// durably committed to the manifest store. Non-acked batches must be
/// re-delivered after a restart.
#[async_trait]
pub trait ChangeFeed: Send + Sync + 'static {
    /// Open a fresh subscription. Each call returns an independent cursor
    /// positioned at the last acknowledged source version (or earliest
    /// available, on first connect).
    async fn subscribe(&self) -> Result<Box<dyn ChangeSubscription>, SourceError>;
}

/// Owned, ack-aware subscription. Pulled by the compiler one batch at a time.
///
/// `acknowledge` MUST only be called once a batch's effects are durably
/// recorded in the manifest store, so a crash between `next_batch` and
/// `acknowledge` re-delivers the batch on reconnect.
#[async_trait]
pub trait ChangeSubscription: Send {
    /// Pull the next committed batch. `None` indicates the feed closed
    /// cleanly; transient errors surface as `Some(Err(_))` and the caller
    /// decides whether to abort.
    async fn next_batch(&mut self) -> Option<Result<ChangeBatch, SourceError>>;

    /// Acknowledge that every batch up to and including `source_version` is
    /// durably persisted downstream. Adapters with no notion of a cursor
    /// (polling fallback) treat this as a no-op.
    async fn acknowledge(&mut self, source_version: Option<&str>) -> Result<(), SourceError>;

    /// Gracefully tear down the subscription, awaiting any background worker
    /// so that a final feedback ack is on the wire before the call returns.
    /// Callers must invoke this on every shutdown path; `Drop` is a fallback
    /// that may abort in-flight work without waiting. Default impl is a no-op
    /// for adapters with no detached workers.
    async fn shutdown(&mut self) -> Result<(), SourceError> {
        Ok(())
    }
}

/// Opaque guard returned by [`LeaderLock::try_acquire`]. Holding it keeps the
/// underlying lock; dropping it releases the lock (typically by closing the
/// session that owns it).
pub trait LeaderLockGuard: Send + Sync + std::fmt::Debug {}

/// Coordination port: at most one compiler instance per service holds the
/// leader lock at a time. The hash is precomputed by the application so the
/// adapter does not need to know about service identity.
#[async_trait]
pub trait LeaderLock: Send + Sync + 'static {
    /// Try to acquire the lock keyed by `key`. Non-blocking:
    /// - `Ok(Some(guard))` — leader; hold `guard` for the duration of work.
    /// - `Ok(None)` — another instance holds the lock.
    /// - `Err(_)` — backend error.
    async fn try_acquire(&self, key: i64) -> Result<Option<Box<dyn LeaderLockGuard>>, SourceError>;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn binding_constructor_accepts_valid() {
        let b = SourceBinding::new(
            SourceCollectionId::new("roads"),
            "public",
            "roads",
            "geom",
            "gid",
            vec!["name".into(), "class".into()],
            CrsCode::new("EPSG:25832"),
        )
        .unwrap();
        assert_eq!(b.collection.as_str(), "roads");
        assert_eq!(b.from_schema, "public");
        assert_eq!(b.from_table, "roads");
        assert_eq!(b.geometry_column, "geom");
        assert_eq!(b.id_column, "gid");
        assert_eq!(b.attributes, vec!["name".to_string(), "class".to_string()]);
        assert_eq!(b.crs.as_str(), "EPSG:25832");
    }

    #[test]
    fn binding_rejects_duplicate_attribute() {
        let r = SourceBinding::new(
            SourceCollectionId::new("c"),
            "s",
            "t",
            "g",
            "id",
            vec!["a".into(), "a".into()],
            CrsCode::new("EPSG:4326"),
        );
        assert!(matches!(r, Err(SourceError::InvalidBinding(_))));
    }

    #[test]
    fn binding_rejects_empty_field() {
        let r = SourceBinding::new(
            SourceCollectionId::new("c"),
            "",
            "t",
            "g",
            "id",
            vec![],
            CrsCode::new("EPSG:4326"),
        );
        assert!(matches!(r, Err(SourceError::InvalidBinding(_))));
    }

    // fake source counting fetch_cell invocations; verifies the default
    // fetch_cells impl fans out one call per (cell, bbox) pair and preserves
    // routing of rows back to their cell.
    struct CountingSource {
        counter: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl Source for CountingSource {
        async fn fetch_cell(
            &self,
            _binding: &SourceBinding,
            cell: &Cell,
            _bbox: Bbox,
            _filter: Option<&Expr>,
        ) -> Result<Vec<RowBytes>, SourceError> {
            self.counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(vec![RowBytes {
                feature_id: cell.x as u64 * 1000 + cell.y as u64,
                geometry: bytes::Bytes::new(),
                attributes: Vec::new(),
            }])
        }
    }

    fn binding_for_test() -> SourceBinding {
        SourceBinding::new(
            SourceCollectionId::new("c"),
            "s",
            "t",
            "g",
            "id",
            vec![],
            CrsCode::new("EPSG:4326"),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn default_fetch_cells_fans_out_to_fetch_cell() {
        let src = CountingSource {
            counter: std::sync::atomic::AtomicUsize::new(0),
        };
        let band = mars_types::ScaleBand::new("hi");
        let bbox = Bbox::new(0.0, 0.0, 1.0, 1.0);
        let cells = vec![
            (
                Cell {
                    band: band.clone(),
                    x: 1,
                    y: 2,
                },
                bbox,
            ),
            (
                Cell {
                    band: band.clone(),
                    x: 3,
                    y: 4,
                },
                bbox,
            ),
            (
                Cell {
                    band: band.clone(),
                    x: 5,
                    y: 6,
                },
                bbox,
            ),
        ];
        let out = src.fetch_cells(&binding_for_test(), &cells, None).await.unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(src.counter.load(std::sync::atomic::Ordering::Relaxed), 3);
        for ((in_cell, _), (out_cell, rows)) in cells.iter().zip(&out) {
            assert_eq!(in_cell, out_cell);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].feature_id, in_cell.x as u64 * 1000 + in_cell.y as u64);
        }
    }
}
