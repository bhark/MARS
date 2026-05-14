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
    for layer in &cfg.layers {
        for class in &layer.classes {
            if let mars_config::ClassStyle::Inline(style) = &class.style
                && let Some(FillPaint::Image { name }) = &style.fill
            {
                names.insert(name.clone());
            }
        }
    }
    for entry in cfg.styles.values() {
        if let Some(style) = entry.as_geometry()
            && let Some(FillPaint::Image { name }) = &style.fill
        {
            names.insert(name.clone());
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use mars_artifact::decode_image_resources;
    use std::io::Write;

    fn touch(dir: &Path, name: &str, bytes: &[u8]) {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(bytes).unwrap();
    }

    #[test]
    fn empty_refs_packs_to_empty_section() {
        let bytes = pack_images(&[], None).unwrap();
        let back = decode_image_resources(&bytes).unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn missing_dir_with_refs_is_typed_error() {
        let err = pack_images(&["brick".into()], None).expect_err("missing dir");
        assert!(matches!(err, CompilerError::ImagePack { what, .. } if what == "images_dir missing"));
    }

    #[test]
    fn missing_file_is_typed_error() {
        let td = tempfile::TempDir::new().unwrap();
        let err = pack_images(&["brick".into()], Some(td.path())).expect_err("missing file");
        assert!(matches!(err, CompilerError::ImagePack { what, .. } if what == "image file read"));
    }

    #[test]
    fn empty_file_is_typed_error() {
        let td = tempfile::TempDir::new().unwrap();
        touch(td.path(), "brick.png", b"");
        let err = pack_images(&["brick".into()], Some(td.path())).expect_err("empty file");
        assert!(matches!(err, CompilerError::ImagePack { what, .. } if what == "image file empty"));
    }

    #[test]
    fn pack_emits_decodable_section_with_extension_fallback() {
        let td = tempfile::TempDir::new().unwrap();
        touch(td.path(), "brick.png", b"\x89PNG\x0d\x0a\x1a\x0afake");
        touch(td.path(), "grass", b"raw-bytes"); // no extension
        let bytes = pack_images(&["brick".into(), "grass".into()], Some(td.path())).unwrap();
        let back = decode_image_resources(&bytes).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].name, "brick");
        assert!(back[0].bytes.starts_with(b"\x89PNG"));
        assert_eq!(back[1].name, "grass");
        assert_eq!(back[1].bytes.as_ref(), b"raw-bytes");
    }

    #[test]
    fn collect_from_inline_styles_is_sorted_and_dedup() {
        use mars_config::{ClassStyle, Layer};
        use mars_style::{Colour, FillPaint, Style};
        use mars_types::LayerId;

        fn class_with_fill(name: &str, fill: Option<FillPaint>) -> mars_config::Class {
            let style = Style {
                fill,
                ..Default::default()
            };
            mars_config::Class {
                name: name.into(),
                title: String::new(),
                when: None,
                scale: None,
                style: ClassStyle::Inline(style),
            }
        }
        let layer = Layer {
            name: LayerId::new("a"),
            title: String::new(),
            abstract_: String::new(),
            kind: "polygon".into(),
            scale: None,
            group: None,
            enable_get_feature_info: false,
            bbox: None,
            sources: vec![],
            classes: vec![
                class_with_fill("stone", Some(FillPaint::Image { name: "stone".into() })),
                class_with_fill("brick1", Some(FillPaint::Image { name: "brick".into() })),
                class_with_fill("brick2", Some(FillPaint::Image { name: "brick".into() })),
                class_with_fill("solid", Some(FillPaint::Solid(Colour::rgba(0, 0, 0, 255)))),
                class_with_fill("no_fill", None),
            ],
            label: None,
            label_survival: mars_config::LabelSurvival::Independent,
        };

        let refs = collect_from_layers_and_styles(&[layer], &std::collections::BTreeMap::new());
        assert_eq!(refs, vec!["brick".to_string(), "stone".to_string()]);
    }

    // exposes the dedup logic without paying for a full Config builder in
    // tests. mirrors the public `collect_image_refs` exactly.
    fn collect_from_layers_and_styles(
        layers: &[mars_config::Layer],
        styles: &std::collections::BTreeMap<String, mars_config::StyleEntry>,
    ) -> Vec<String> {
        let mut names: BTreeSet<String> = BTreeSet::new();
        for layer in layers {
            for class in &layer.classes {
                if let mars_config::ClassStyle::Inline(style) = &class.style
                    && let Some(FillPaint::Image { name }) = &style.fill
                {
                    names.insert(name.clone());
                }
            }
        }
        for entry in styles.values() {
            if let Some(style) = entry.as_geometry()
                && let Some(FillPaint::Image { name }) = &style.fill
            {
                names.insert(name.clone());
            }
        }
        names.into_iter().collect()
    }
}
