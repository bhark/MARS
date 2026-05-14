//! Manifest-bound image registry: decoded RGBA bitmaps keyed by name,
//! refreshed atomically alongside the rest of [`RuntimeState`] on each
//! manifest swap.
//!
//! Lives in mars-runtime (not in the renderer adapter) because the lifetime
//! is owned by manifest swaps. The renderer holds an `Arc<dyn ImageRegistry>`
//! for the duration of the process and never recreates itself; this struct
//! gives the runtime a swap-able lookup behind that handle.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use mars_artifact::decode_image_resources;
use mars_render_port::{DecodedImage, ImageRegistry};
use mars_store::{LocalCache, ObjectStore};
use mars_types::ArtifactEntry;

use crate::RuntimeError;
use crate::decode::decode_png_to_rgba;

/// Mutable image registry. The renderer holds an `Arc<MutableImageRegistry>`
/// (cast to `dyn ImageRegistry`); the runtime calls [`Self::set`] from its
/// state-swap path so manifest updates take effect without rebuilding the
/// renderer.
#[derive(Debug, Default)]
pub struct MutableImageRegistry {
    inner: ArcSwap<HashMap<String, Arc<DecodedImage>>>,
}

impl MutableImageRegistry {
    /// Build an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: ArcSwap::from_pointee(HashMap::new()),
        }
    }

    /// Atomically replace the registry contents.
    pub fn set(&self, entries: HashMap<String, Arc<DecodedImage>>) {
        self.inner.store(Arc::new(entries));
    }

    /// Number of entries currently bound. Cheap; reads the current snapshot.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.load().len()
    }

    /// `true` when the registry holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.load().is_empty()
    }
}

impl ImageRegistry for MutableImageRegistry {
    fn get(&self, name: &str) -> Option<Arc<DecodedImage>> {
        self.inner.load().get(name).cloned()
    }
}

/// Fetch the manifest's `image_artifact`, decode every entry, and build a
/// HashMap suitable for [`MutableImageRegistry::set`]. Returns an empty map
/// when the manifest carries `image_artifact: None`.
///
/// PNG is the only decoder wired today; non-PNG bytes surface as a typed
/// error so the contract is visible. Adding webp / jpeg is a deps-only
/// follow-up (the artifact section is format-agnostic).
pub async fn load_from_manifest(
    image_artifact: Option<&ArtifactEntry>,
    cache: &Arc<dyn LocalCache>,
    store: &Arc<dyn ObjectStore>,
) -> Result<HashMap<String, Arc<DecodedImage>>, RuntimeError> {
    let Some(entry) = image_artifact else {
        return Ok(HashMap::new());
    };
    let key = entry.key.clone();
    let bytes = cache.get_or_fetch(&key, entry.hash, store.as_ref()).await?;
    let resources = decode_image_resources(&bytes).map_err(|e| RuntimeError::InvalidManifest {
        reason: format!("image_artifact section decode: {e}"),
    })?;
    let mut out = HashMap::with_capacity(resources.len());
    for r in resources {
        let decoded = decode_png_to_rgba(&r.bytes).map_err(|e| RuntimeError::InvalidManifest {
            reason: format!("image '{}' decode: {e}", r.name),
        })?;
        out.insert(r.name, Arc::new(decoded));
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn img(byte: u8) -> Arc<DecodedImage> {
        Arc::new(DecodedImage {
            width: 1,
            height: 1,
            rgba: Arc::new(vec![byte, byte, byte, 255]),
        })
    }

    #[test]
    fn empty_registry_returns_none() {
        let reg = MutableImageRegistry::new();
        assert!(reg.is_empty());
        assert!(reg.get("brick").is_none());
    }

    #[test]
    fn set_then_get_returns_entry() {
        let reg = MutableImageRegistry::new();
        let mut map = HashMap::new();
        map.insert("brick".to_string(), img(1));
        reg.set(map);
        assert_eq!(reg.len(), 1);
        let got = reg.get("brick").expect("present");
        assert_eq!(got.rgba.as_slice(), &[1, 1, 1, 255]);
    }

    #[test]
    fn second_set_replaces_prior() {
        let reg = MutableImageRegistry::new();
        let mut a = HashMap::new();
        a.insert("brick".into(), img(1));
        reg.set(a);
        let mut b = HashMap::new();
        b.insert("stone".into(), img(2));
        reg.set(b);
        assert!(reg.get("brick").is_none());
        assert_eq!(reg.get("stone").expect("stone").rgba.as_slice(), &[2, 2, 2, 255]);
    }
}
