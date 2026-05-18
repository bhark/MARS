#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

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
            style: ClassStyle::Inline(Box::new(style)),
            label: None,
        }
    }
    let layer = Layer {
        name: LayerId::new("a"),
        title: String::new(),
        abstract_: String::new(),
        kind: "polygon".into(),
        scale: None,
        group: None,
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
        raster: None,
        wms: Default::default(),
        ows: Default::default(),
        template: None,
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
    let visit = |style: &mars_style::Style, names: &mut BTreeSet<String>| {
        if let Some(FillPaint::Image { name }) = &style.fill {
            names.insert(name.clone());
        }
    };
    for layer in layers {
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
    for entry in styles.values() {
        if let Some(passes) = entry.as_geometry_passes() {
            for s in passes {
                visit(s, &mut names);
            }
        }
    }
    names.into_iter().collect()
}
