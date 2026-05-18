//! every emitted golden parses + validates as a `RenderDefinition`.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;

use mars_config::RenderDefinition;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

#[test]
fn every_golden_round_trips_through_render_definition() {
    let dir = fixture_dir();
    let entries = std::fs::read_dir(&dir).expect("read fixture dir");

    let mut goldens: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".expected.yaml"))
        })
        .collect();
    goldens.sort();

    assert!(!goldens.is_empty(), "no goldens found in {}", dir.display());

    let mut failures: Vec<String> = Vec::new();
    for path in &goldens {
        let yaml = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        match RenderDefinition::from_yaml(&yaml) {
            Err(e) => failures.push(format!("parse  {}: {e}", path.display())),
            Ok(mut def) => {
                if let Err(e) = def.validate() {
                    failures.push(format!("valid {}: {e}", path.display()));
                }
            }
        }
    }
    assert!(failures.is_empty(), "round-trip failures:\n  {}", failures.join("\n  "));
}
