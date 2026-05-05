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
use futures_core::stream::BoxStream;
use mars_expr::Expr;
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
        collection: String,
        /// Cells touched by the new geometry.
        cells: Vec<Cell>,
    },
    /// A row was updated; cells = touched-by-old ∪ touched-by-new.
    Update {
        /// Logical name of the source table / collection.
        collection: String,
        /// Cells touched by the old or new geometry.
        cells: Vec<Cell>,
    },
    /// A row was deleted; cells = touched-by-old.
    Delete {
        /// Logical name of the source table / collection.
        collection: String,
        /// Cells touched by the old geometry.
        cells: Vec<Cell>,
    },
    /// The whole collection was truncated; every cell is dirty.
    Truncate {
        /// Logical name of the source table / collection.
        collection: String,
    },
}

/// Stable identifier for a source collection (logical name shared between
/// the binding, change feed, and compiled artifact metadata).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceCollectionId(String);

impl SourceCollectionId {
    /// Construct from any string-like value.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Borrow as a `&str`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SourceCollectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
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
    use super::{ChangeEvent, ChangeFeed, RowBytes, Source, SourceBinding, SourceError};
    use async_trait::async_trait;
    use futures_core::stream::BoxStream;
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
        async fn subscribe(&self) -> Result<BoxStream<'static, Result<ChangeEvent, SourceError>>, SourceError> {
            Err(SourceError::NotImplemented {
                what: "mars-source::stub::NotImplementedSource::subscribe",
            })
        }
    }
}

/// Subscription-side port: a stream of `ChangeEvent`s.
#[async_trait]
pub trait ChangeFeed: Send + Sync + 'static {
    /// Subscribe to the change feed. The returned stream lives for the
    /// lifetime of the compiler process; transient errors are reported
    /// inline as `Result::Err`.
    async fn subscribe(&self) -> Result<BoxStream<'static, Result<ChangeEvent, SourceError>>, SourceError>;
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
}
