//! image_pack helper: collect `FillPaint::Image { name }` references from a
//! config, read their bitmap bytes from a configured directory, and assemble
//! an [`ImageResources`](mars_artifact::ImageResource) artifact body that
//! the publish path stamps into the manifest's `image_artifact` slot.
//!
//! The publish-side wiring (object-store upload + `ArtifactEntry`
//! construction) lives at the call site so this module stays a pure pack
//! helper: deterministic, no I/O beyond reading the supplied paths.

use std::collections::BTreeSet;
use std::path::Path;

use bytes::Bytes;
use mars_artifact::{ImageResource, compute_content_hash, encode_image_resources};
use mars_config::Config;
use mars_store::ObjectStore;
use mars_style::FillPaint;
use mars_types::{ArtifactEntry, ArtifactKey, ContentHash};

use crate::CompilerError;

/// Walk every layer's classes and the named stylesheet, collecting unique
/// image-fill names in lexicographic order. Stable order matters because
/// the encoded section is keyed by ascending name for binary search.
#[must_use]
pub(crate) fn collect_image_refs(cfg: &Config) -> Vec<String> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    let visit = |style: &mars_style::Style, names: &mut BTreeSet<String>| {
        if let Some(FillPaint::Image { name }) = &style.fill {
            names.insert(name.clone());
        }
    };
    for layer in &cfg.layers {
        for class in &layer.classes {
            match &class.style {
                mars_config::ClassStyle::Inline(style) => visit(style, &mut names),
                mars_config::ClassStyle::Passes { passes } => {
                    for s in passes {
                        visit(s, &mut names);
                    }
                }
                mars_config::ClassStyle::Ref { .. } => {}
            }
        }
    }
    for entry in cfg.styles.values() {
        if let Some(passes) = entry.as_geometry_passes() {
            for s in passes {
                visit(s, &mut names);
            }
        }
    }
    names.into_iter().collect()
}

/// Read every named bitmap from `images_dir` and assemble an
/// [`ImageResources`] section payload. When `refs` is empty the caller
/// should skip publishing an image artifact; this helper still returns an
/// empty section so the writer remains uniform.
///
/// Errors:
/// - `images_dir` is `None` while `refs` is non-empty -> typed error.
/// - A referenced file is missing or unreadable -> typed error.
/// - The artifact codec rejected the assembled payload -> typed error.
pub(crate) fn pack_images(refs: &[String], images_dir: Option<&Path>) -> Result<Bytes, CompilerError> {
    if refs.is_empty() {
        return encode_image_resources(&[]).map_err(|e| CompilerError::ImagePack {
            what: "section encode",
            detail: e.to_string(),
        });
    }
    let dir = images_dir.ok_or_else(|| CompilerError::ImagePack {
        what: "images_dir missing",
        detail: format!(
            "config references {} image{} but compiler.images_dir is unset",
            refs.len(),
            if refs.len() == 1 { "" } else { "s" }
        ),
    })?;
    let mut entries = Vec::with_capacity(refs.len());
    for name in refs {
        let path = resolve_image_path(dir, name);
        let bytes = std::fs::read(&path).map_err(|e| CompilerError::ImagePack {
            what: "image file read",
            detail: format!("{}: {e}", path.display()),
        })?;
        if bytes.is_empty() {
            return Err(CompilerError::ImagePack {
                what: "image file empty",
                detail: path.display().to_string(),
            });
        }
        entries.push(ImageResource {
            name: name.clone(),
            bytes: Bytes::from(bytes),
        });
    }
    encode_image_resources(&entries).map_err(|e| CompilerError::ImagePack {
        what: "section encode",
        detail: e.to_string(),
    })
}

/// Build the image pack from the config and publish it to the object store,
/// returning an [`ArtifactEntry`] for the manifest's `image_artifact` slot.
/// Returns `Ok(None)` when the config references no images (the manifest
/// then carries `image_artifact: None`).
pub(crate) async fn publish_image_artifact(
    cfg: &Config,
    store: &dyn ObjectStore,
) -> Result<Option<ArtifactEntry>, CompilerError> {
    let refs = collect_image_refs(cfg);
    if refs.is_empty() {
        return Ok(None);
    }
    let images_dir = cfg.compiler.images_dir.as_deref().map(Path::new);
    let bytes = pack_images(&refs, images_dir)?;
    let hash = compute_content_hash(&bytes);
    let size_bytes = bytes.len() as u64;
    let key = image_pack_object_key(&hash);
    store.put(&key, bytes).await.map_err(|e| CompilerError::ImagePack {
        what: "object-store put",
        detail: e.to_string(),
    })?;
    Ok(Some(ArtifactEntry { key, hash, size_bytes }))
}

fn image_pack_object_key(hash: &ContentHash) -> ArtifactKey {
    ArtifactKey::new(format!("images/{hex}.pack", hex = hash.to_hex()))
}

// resolve `<dir>/<name>` first; if no extension, try common bitmap
// extensions in order (png, jpg, jpeg, webp) so the mapfile / yaml author
// can omit the extension. picks the first that exists.
fn resolve_image_path(dir: &Path, name: &str) -> std::path::PathBuf {
    let direct = dir.join(name);
    if direct.exists() {
        return direct;
    }
    for ext in ["png", "jpg", "jpeg", "webp"] {
        let candidate = dir.join(format!("{name}.{ext}"));
        if candidate.exists() {
            return candidate;
        }
    }
    direct
}

#[cfg(test)]
mod tests;
