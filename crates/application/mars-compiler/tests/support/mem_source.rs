//! in-memory `Source` for snapshot integration tests.

use std::collections::HashMap;

use async_trait::async_trait;
use bytes::Bytes;
use mars_expr::Expr;
use mars_source::{ChangeFeed, ChangeSubscription, RowBytes, Source, SourceBinding, SourceCollectionId, SourceError};
use mars_types::{Bbox, Cell};

#[derive(Debug, Default)]
pub(crate) struct MemSource {
    pub(crate) rows: HashMap<(SourceCollectionId, Cell), Vec<RowBytes>>,
}

impl MemSource {
    pub(crate) fn insert(&mut self, collection: SourceCollectionId, cell: Cell, rows: Vec<RowBytes>) {
        self.rows.insert((collection, cell), rows);
    }
}

#[async_trait]
impl Source for MemSource {
    async fn fetch_cell(
        &self,
        binding: &SourceBinding,
        cell: &Cell,
        _bbox: Bbox,
        _filter: Option<&Expr>,
    ) -> Result<Vec<RowBytes>, SourceError> {
        Ok(self
            .rows
            .get(&(binding.collection.clone(), cell.clone()))
            .cloned()
            .unwrap_or_default())
    }
}

#[async_trait]
impl ChangeFeed for MemSource {
    async fn subscribe(&self) -> Result<Box<dyn ChangeSubscription>, SourceError> {
        Err(SourceError::NotImplemented {
            what: "MemSource::subscribe",
        })
    }
}

/// little-endian WKB polygon (4-vertex closed ring) at the given offsets.
#[must_use]
pub(crate) fn wkb_polygon(coords: &[(f64, f64)]) -> Bytes {
    let mut v = Vec::new();
    v.push(1u8); // little-endian
    v.extend_from_slice(&3u32.to_le_bytes()); // wkb polygon
    v.extend_from_slice(&1u32.to_le_bytes()); // 1 ring
    v.extend_from_slice(&(coords.len() as u32).to_le_bytes());
    for (x, y) in coords {
        v.extend_from_slice(&x.to_le_bytes());
        v.extend_from_slice(&y.to_le_bytes());
    }
    Bytes::from(v)
}
