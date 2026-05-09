//! integration tests: run the built binary against the bundled fixture.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;
use std::process::Command;

fn bin_path() -> PathBuf {
    // CARGO_BIN_EXE_<name> is set by cargo test for the binary target.
    PathBuf::from(env!("CARGO_BIN_EXE_mars-import-mapfile"))
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/minimal.map")
}

#[test]
fn produces_expected_skeleton() {
    let out = Command::new(bin_path()).arg(fixture()).output().expect("run binary");
    assert!(out.status.success(), "non-strict run should succeed");
    let s = String::from_utf8(out.stdout).expect("utf8");
    assert!(s.contains("service:"), "missing service: -- {s}");
    assert!(s.contains("name: \"test\""), "missing service name -- {s}");
    assert!(
        s.contains("experimental scaffold") && s.contains("not a production config"),
        "output must self-identify as a non-production scaffold -- {s}"
    );
    for layer in ["roads", "buildings", "labels"] {
        assert!(
            s.contains(&format!("hand-tune layer {layer}")),
            "missing per-layer hand-tune marker for {layer} -- {s}"
        );
    }
}

#[test]
fn strict_exits_two_on_unsupported() {
    let out = Command::new(bin_path())
        .arg("--strict")
        .arg(fixture())
        .output()
        .expect("run binary");
    assert_eq!(
        out.status.code(),
        Some(2),
        "expected exit 2 in strict mode (fixture has SYMBOL); got {:?} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
}
