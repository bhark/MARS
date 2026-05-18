//! Vector source binding: logical collection to backend locator + projection.

use mars_types::CrsCode;

use crate::{SourceCollectionId, SourceError};

/// Source-side binding: maps a logical `SourceCollectionId` onto a backend
/// locator, geometry/id fields, attribute projection, and CRS. Lives in
/// `mars-source` because every field is a backend-shaped locator, not domain.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceBinding {
    /// Logical collection name.
    pub collection: SourceCollectionId,
    /// Opaque backend-side locator for the source records. Format is defined
    /// by the adapter (e.g. `"schema.table"` for relational backends, a
    /// collection path for document stores, an object-store prefix, etc.).
    pub from: String,
    /// Geometry field name within each record.
    pub geometry_field: String,
    /// Stable feature-id field name within each record.
    pub id_field: String,
    /// Ordered, deduplicated attribute projection.
    pub attributes: Vec<String>,
    /// Source CRS.
    pub crs: CrsCode,
    /// Optional binding-level filter applied to every scan this binding
    /// drives. Idents must already be in `attributes ∪ {id_field}` (the
    /// caller is expected to validate up front; lowering double-checks).
    pub filter: Option<mars_expr::Expr>,
    /// Optional adapter-side DSN / connection-string override. Currently
    /// honoured only by the postgis adapter, which routes the binding to a
    /// per-DSN connection pool. Vector-file and other adapters ignore this
    /// field. Carried on the port binding (rather than out-of-band) so the
    /// adapter sees the override at the same boundary it sees the locator.
    pub dsn: Option<String>,
}

impl SourceBinding {
    /// Construct after validating attribute uniqueness and non-empty
    /// locator / field names.
    pub fn new(
        collection: SourceCollectionId,
        from: impl Into<String>,
        geometry_field: impl Into<String>,
        id_field: impl Into<String>,
        attributes: Vec<String>,
        crs: CrsCode,
    ) -> Result<Self, SourceError> {
        let from = from.into();
        let geometry_field = geometry_field.into();
        let id_field = id_field.into();

        for (label, v) in [
            ("from", &from),
            ("geometry_field", &geometry_field),
            ("id_field", &id_field),
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
            from,
            geometry_field,
            id_field,
            attributes,
            crs,
            filter: None,
            dsn: None,
        })
    }

    /// Attach (or clear) a binding-level filter expression. Lowering applies
    /// it to the source scan in addition to any caller-supplied filter.
    #[must_use]
    pub fn with_filter(mut self, filter: Option<mars_expr::Expr>) -> Self {
        self.filter = filter;
        self
    }

    /// Attach (or clear) a binding-level DSN override. Honoured by adapters
    /// that route per-DSN (postgis); ignored elsewhere.
    #[must_use]
    pub fn with_dsn(mut self, dsn: Option<String>) -> Self {
        self.dsn = dsn;
        self
    }
}

#[cfg(test)]
mod tests;
