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
    let bytes = cache
        .get_or_fetch(&key, page.content_hash, origin.as_ref())
        .await?;
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
    let bytes = cache
        .get_or_fetch(&key, entry.content_hash, origin.as_ref())
        .await?;
    Ok(bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use mars_store::mem::{InMemoryCache, InMemoryStore};
    use mars_store::{LocalCache, ObjectStore};
    use mars_types::{
        Bbox, BindingId, ContentHash, DecimationLevel, HilbertKey, LayerId, LayerSidecarKind,
        PageEntry, PageId, PageKey,
    };

    use super::*;

    async fn put_blob(store: &Arc<dyn ObjectStore>, key_str: &str, body: &[u8]) -> ContentHash {
        let key = mars_types::ArtifactKey::new(key_str);
        store.put(&key, Bytes::copy_from_slice(body)).await.unwrap()
    }

    fn page_for(binding: &str, level: u8, page_id: u64, hash: ContentHash) -> PageEntry {
        PageEntry {
            key: PageKey {
                binding_id: BindingId::try_new(binding).unwrap(),
                level: DecimationLevel::new(level),
                page_id: PageId::new(page_id),
            },
            content_hash: hash,
            spatial_bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
            hilbert_range: (HilbertKey::new(0), HilbertKey::new(1)),
            feature_count: 0,
            size_bytes: body_len(),
        }
    }

    fn body_len() -> u64 {
        // helper kept so the test reads at the call sites without inlining
        // a magic number.
        7
    }

    fn class_sidecar(layer: &str, page_key: PageKey, hash: ContentHash) -> LayerSidecarEntry {
        LayerSidecarEntry {
            layer_id: LayerId::new(layer),
            page_key,
            content_hash: hash,
            size_bytes: body_len(),
            kind: LayerSidecarKind::Class,
        }
    }

    #[tokio::test]
    async fn fetch_page_round_trips_via_cache() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemoryStore::new());
        let cache: Arc<dyn LocalCache> = Arc::new(InMemoryCache::new());
        let body = b"PAGE000";
        let hash = put_blob(&store, "bnd/a/L0/p0000000000000001/", body).await;
        // overwrite with the canonical key the helper derives:
        let entry = page_for("a", 0, 1, hash);
        let real_key = entry.key.object_key(&entry.content_hash).unwrap();
        store
            .put(&real_key, Bytes::copy_from_slice(body))
            .await
            .unwrap();
        let got = fetch_page(&cache, &store, &entry).await.unwrap();
        assert_eq!(got.as_ref(), body);
    }

    #[tokio::test]
    async fn fetch_sidecar_round_trips_via_cache() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemoryStore::new());
        let cache: Arc<dyn LocalCache> = Arc::new(InMemoryCache::new());
        let body = b"SIDECAR";
        // populate the canonical key the entry derives.
        let page_key = PageKey {
            binding_id: BindingId::try_new("a").unwrap(),
            level: DecimationLevel::new(0),
            page_id: PageId::new(1),
        };
        let placeholder = class_sidecar("layer-a", page_key.clone(), ContentHash::zero());
        let real_key = placeholder.object_key().unwrap();
        let hash = store
            .put(&real_key, Bytes::copy_from_slice(body))
            .await
            .unwrap();
        let entry = class_sidecar("layer-a", page_key, hash);
        let real_key = entry.object_key().unwrap();
        store
            .put(&real_key, Bytes::copy_from_slice(body))
            .await
            .unwrap();
        let got = fetch_sidecar(&cache, &store, &entry).await.unwrap();
        assert_eq!(got.as_ref(), body);
    }

    #[tokio::test]
    async fn fetch_page_propagates_hash_mismatch() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemoryStore::new());
        let cache: Arc<dyn LocalCache> = Arc::new(InMemoryCache::new());
        let body = b"PAGE001";
        // populate at the right key but keep the manifest's expected hash
        // pointing somewhere else; the cache must fail closed.
        let entry = page_for("a", 0, 2, ContentHash([1u8; 32]));
        let real_key = entry.key.object_key(&entry.content_hash).unwrap();
        store
            .put(&real_key, Bytes::copy_from_slice(body))
            .await
            .unwrap();
        let err = fetch_page(&cache, &store, &entry).await.unwrap_err();
        match err {
            RuntimeError::Store(mars_store::StoreError::HashMismatch { .. }) => {}
            other => panic!("expected HashMismatch, got {other:?}"),
        }
    }
}
