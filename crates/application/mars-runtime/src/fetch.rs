//! page and sidecar fetch path.
//!
//! thin async wrappers around the [`LocalCache`] port. callers pass
//! `PageEntry` / `LayerSidecarEntry` references straight from the manifest;
//! the helpers derive the canonical object key, fan out through the cache
//! (mmap-friendly), and surface verification failures as [`RuntimeError`].
//!
//! orchestration (per-layer fan-out via `FuturesUnordered`) lives in
//! `Runtime::render` (D4); this module is one fetch per call so the unit
//! seam stays simple and testable.

use std::sync::Arc;

use bytes::Bytes;
use mars_store::{LocalCache, ObjectStore};
use mars_types::{LayerSidecarEntry, PageEntry};

use crate::RuntimeError;

/// fetch the bytes for a page artifact via the local cache, falling back to
/// `origin` on miss. the cache verifies the content hash for us; corruption
/// surfaces as [`mars_store::StoreError::HashMismatch`] -> [`RuntimeError`].
pub async fn fetch_page(
    cache: &Arc<dyn LocalCache>,
    origin: &Arc<dyn ObjectStore>,
    page: &PageEntry,
) -> Result<Bytes, RuntimeError> {
    let key = page
        .key
        .object_key(&page.content_hash)
        .map_err(|e| RuntimeError::InvalidManifest {
            reason: format!("malformed page object key for {:?}: {e}", page.key),
        })?;
    let bytes = cache.get_or_fetch(&key, page.content_hash, origin.as_ref()).await?;
    Ok(bytes)
}

/// fetch the bytes for a class or label sidecar; the kind is implicit in the
/// entry's `kind` field and surfaces in the canonical key prefix
/// (`cls/...` vs `lbl/...`).
pub async fn fetch_sidecar(
    cache: &Arc<dyn LocalCache>,
    origin: &Arc<dyn ObjectStore>,
    entry: &LayerSidecarEntry,
) -> Result<Bytes, RuntimeError> {
    let key = entry.object_key().map_err(|e| RuntimeError::InvalidManifest {
        reason: format!(
            "malformed sidecar object key for layer {} at {:?}: {e}",
            entry.layer_id, entry.page_key
        ),
    })?;
    let bytes = cache.get_or_fetch(&key, entry.content_hash, origin.as_ref()).await?;
    Ok(bytes)
}

#[cfg(test)]
mod tests;
