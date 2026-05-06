//! CLI smoke tests for `mars validate` and `mars inspect`. Doesn't touch
//! Postgres or Docker; runs in milliseconds and gates against regressions in
//! the operational tooling subcommands.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::process::Command;

fn mars_bin() -> &'static str {
    // cargo provides this env var for integration tests of binary crates.
    env!("CARGO_BIN_EXE_mars")
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

#[test]
fn validate_accepts_committed_fixture() {
    let fixture = workspace_root().join("crates/support/mars-config/tests/fixtures/demo_minimal.yaml");
    assert!(fixture.exists(), "fixture missing: {}", fixture.display());
    let out = Command::new(mars_bin())
        .arg("validate")
        .arg(&fixture)
        .output()
        .expect("spawn mars validate");
    assert!(
        out.status.success(),
        "validate failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("ok"));
}

#[test]
fn validate_rejects_malformed() {
    let tmp = tempfile::tempdir().unwrap();
    let bad = tmp.path().join("bad.yaml");
    std::fs::write(&bad, "service:\n  name: ''\n").unwrap();
    let out = Command::new(mars_bin())
        .arg("validate")
        .arg(&bad)
        .output()
        .expect("spawn mars validate");
    assert!(!out.status.success(), "validate accepted malformed config");
}

#[test]
fn inspect_accepts_synthetic_artifact() {
    use mars_artifact::{ArtifactKind, ArtifactWriter, FeatureGeom, GeomKind};
    use mars_types::Bbox;

    let mut w = ArtifactWriter::new(ArtifactKind::Source);
    w.add_geometry_payload(vec![FeatureGeom {
        id: 1,
        bbox: [0.0, 0.0, 1.0, 1.0],
        geom: GeomKind::Point((0.5, 0.5)),
    }])
    .set_bbox(Bbox::new(0.0, 0.0, 1.0, 1.0))
    .set_feature_count(1);
    let bytes = w.finish().expect("finish");

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("synthetic.mars");
    std::fs::write(&path, bytes).unwrap();

    let out = Command::new(mars_bin())
        .arg("inspect")
        .arg(&path)
        .output()
        .expect("spawn mars inspect");
    assert!(
        out.status.success(),
        "inspect failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("kind: Source"), "unexpected output: {stdout}");
    assert!(stdout.contains("feature_count: 1"));
}
