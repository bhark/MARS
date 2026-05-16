#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Smoke-test that the in-repo preset packs parse against the current
//! `StyleEntry` schema. Catches drift if the schema evolves without the
//! pack getting refreshed.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use mars_config::StyleEntry;

fn presets_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("presets")
}

#[test]
fn symbols_pack_parses_as_style_entry_map() {
    let path = presets_dir().join("symbols.yaml");
    let yaml = fs::read_to_string(&path).expect("read presets/symbols.yaml");
    let entries: BTreeMap<String, StyleEntry> =
        serde_yaml_ng::from_str(&yaml).expect("preset pack parses as map of StyleEntry");
    // sanity: the pack ships the variants the TODO calls out.
    for name in [
        "preset_circle_filled",
        "preset_circle_hollow",
        "preset_square_filled",
        "preset_square_hollow",
        "preset_triangle_filled",
        "preset_triangle_hollow",
        "preset_cross",
        "preset_x",
        "preset_pin",
        "preset_diamond_filled",
        "preset_star_filled",
    ] {
        assert!(
            entries.contains_key(name),
            "preset pack is missing required entry `{name}`"
        );
    }
    // every entry must be a point style (markers are point-style).
    for (name, entry) in &entries {
        let passes = entry
            .as_geometry_passes()
            .unwrap_or_else(|| panic!("preset `{name}` should expose geometry passes"));
        assert!(!passes.is_empty(), "preset `{name}` has no passes");
        assert!(passes[0].marker.is_some(), "preset `{name}` must carry a marker");
    }
}
