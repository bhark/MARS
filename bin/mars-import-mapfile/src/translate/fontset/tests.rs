#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn parse_fontset_text_skips_comments_and_blanks() {
    let src = "# top comment\n\nsans dejavu-sans.ttf\nserif dejavu-serif.ttf  # trailing\n  \n";
    let pairs = parse_fontset_text(src);
    assert_eq!(
        pairs,
        vec![
            ("sans".into(), "dejavu-sans.ttf".into()),
            ("serif".into(), "dejavu-serif.ttf".into()),
        ]
    );
}

#[test]
fn parse_fontset_text_drops_one_token_lines() {
    let src = "sans\nbold bold.ttf\n";
    let pairs = parse_fontset_text(src);
    assert_eq!(pairs, vec![("bold".into(), "bold.ttf".into())]);
}

#[test]
fn family_name_for_file_resolves_bundled_dejavu() {
    // reuse the bundled dejavu sans from mars-text's test_fonts so the test
    // is independent of system fontconfig.
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/support/mars-text/test_fonts/DejaVuSans.ttf");
    if !path.exists() {
        // build matrix without the sibling crate's resources should not break
        // this test; skip gracefully.
        return;
    }
    let family = family_name_for_file(&path).expect("dejavu sans resolves");
    assert!(
        family.to_ascii_lowercase().contains("dejavu"),
        "expected dejavu family, got {family:?}",
    );
}
