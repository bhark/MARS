//! integration tests: run the built binary against the bundled fixtures.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;
use std::process::Command;

fn bin_path() -> PathBuf {
    // CARGO_BIN_EXE_<name> is set by cargo test for the binary target.
    PathBuf::from(env!("CARGO_BIN_EXE_mars-import-mapfile"))
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

#[test]
fn produces_expected_yaml() {
    let expected_path = fixture_dir().join("minimal.expected.yaml");
    let expected = std::fs::read_to_string(&expected_path).expect("read minimal.expected.yaml");

    let out = Command::new(bin_path())
        .arg(fixture_dir().join("minimal.map"))
        .output()
        .expect("run binary");
    assert!(out.status.success(), "non-strict run should succeed");

    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(
        stdout.trim(),
        expected.trim(),
        "stdout does not match {}. To regenerate: cargo run -p mars-import-mapfile -- tests/fixtures/minimal.map > tests/fixtures/minimal.expected.yaml",
        expected_path.display()
    );
}

#[test]
fn strict_exits_two_on_unsupported() {
    let out = Command::new(bin_path())
        .arg("--strict")
        .arg(fixture_dir().join("strict.map"))
        .output()
        .expect("run binary");
    assert_eq!(
        out.status.code(),
        Some(2),
        "expected exit 2 in strict mode (fixture has COMPOSITE); got {:?} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
}
