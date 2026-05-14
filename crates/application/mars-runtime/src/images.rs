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

fn decode_png_to_rgba(bytes: &[u8]) -> Result<DecodedImage, String> {
    let dec = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = dec.read_info().map_err(|e| format!("png header: {e}"))?;
    let buf_size = reader
        .output_buffer_size()
        .ok_or_else(|| "png buffer size unknown".to_string())?;
    let mut buf = vec![0u8; buf_size];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("png frame: {e}"))?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity(buf.len() / 3 * 4);
            for px in buf.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity(buf.len() * 4);
            for &g in &buf {
                out.extend_from_slice(&[g, g, g, 255]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity(buf.len() * 2);
            for px in buf.chunks_exact(2) {
                out.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
            out
        }
        other => return Err(format!("unsupported png colour type {other:?}")),
    };
    Ok(DecodedImage {
        width: info.width,
        height: info.height,
        rgba: Arc::new(rgba),
    })
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
