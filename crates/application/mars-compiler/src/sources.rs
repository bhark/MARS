//! Source-adapter registry: maps a configured `SourceId` to its
//! constructed `Arc<dyn Source>`. Built by the composition root (bins) and
//! consumed by `Deps::source_for` when a binding plan needs to fetch rows.

use std::collections::BTreeMap;
use std::sync::Arc;

use mars_config::SourceId;
use mars_source::Source;

/// Lookup table mapping a configured source id to its constructed adapter.
/// Bins build one of these via composition and the compiler routes each
/// binding through its declared source.
#[derive(Clone, Default)]
pub struct SourceRegistry {
    by_id: BTreeMap<SourceId, Arc<dyn Source>>,
}

impl SourceRegistry {
    /// Empty registry. Use [`Self::insert`] to populate.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a source. Replaces any prior entry for the same id.
    pub fn insert(&mut self, id: SourceId, source: Arc<dyn Source>) {
        self.by_id.insert(id, source);
    }

    /// Borrow the source registered under `id`, if any.
    #[must_use]
    pub fn get(&self, id: &SourceId) -> Option<Arc<dyn Source>> {
        self.by_id.get(id).cloned()
    }

    /// True when no sources are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Number of registered sources.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Iterate over `(id, source)` pairs in id-ascending order.
    pub fn iter(&self) -> impl Iterator<Item = (&SourceId, &Arc<dyn Source>)> {
        self.by_id.iter()
    }
}

impl std::fmt::Debug for SourceRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceRegistry")
            .field("ids", &self.by_id.keys().collect::<Vec<_>>())
            .finish()
    }
}
