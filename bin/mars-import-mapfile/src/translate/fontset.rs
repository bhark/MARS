//! FONTSET parser and alias-to-family resolver.
//!
//! mapfile carries fonts via `FONTSET "path/to/fontset.txt"` at MAP scope.
//! `fontset.txt` is a flat key/value table: each non-comment line declares
//! `<alias> <filename>` separated by whitespace. Subsequent `LABEL.FONT
//! "<alias>"` and `SYMBOL TYPE TRUETYPE FONT "<alias>"` references use the
//! alias; the renderer needs an actual font family name.
//!
//! The importer resolves each alias to the TTF's declared family name (via
//! `fontdb`) at translate time, then rewrites every alias reference in the
//! emitted skeleton. Unresolvable aliases fall back to the alias verbatim so
//! the YAML always parses; a warn surfaces the gap.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use fontdb::Database;
use tracing::warn;

/// alias -> resolved family-name map. populated from `fontset.txt`; consumed
/// by the emit-time rewrite walking `Skeleton`.
#[derive(Debug, Default, Clone)]
pub(crate) struct FontAliases {
    map: HashMap<String, String>,
}

impl FontAliases {
    pub(crate) fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// resolve `alias` to the underlying family name; returns `None` when no
    /// entry exists for the alias.
    pub(crate) fn resolve(&self, alias: &str) -> Option<&str> {
        self.map.get(alias).map(String::as_str)
    }
}

/// parse a `fontset.txt` body into `alias -> filename` pairs. blank lines and
/// `#`-comment lines are skipped; lines with only one token are dropped (with
/// a warn) to mirror MapServer's lenient scanner.
fn parse_fontset_text(src: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (idx, raw) in src.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = raw.split('#').next().unwrap_or("").trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut it = trimmed.split_whitespace();
        let alias = match it.next() {
            Some(a) => a.to_string(),
            None => continue,
        };
        let filename = match it.next() {
            Some(f) => f.to_string(),
            None => {
                warn!(line = line_no, alias = %alias, "fontset entry missing filename; skipped");
                continue;
            }
        };
        out.push((alias, filename));
    }
    out
}

/// extract the first family name declared by a TTF/OTF file. uses `fontdb` so
/// the discovery path matches the runtime's font resolution.
fn family_name_for_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let mut db = Database::new();
    db.load_font_data(bytes);
    let face = db.faces().next()?;
    face.families.first().map(|(name, _)| name.clone())
}

/// load a fontset.txt sitting at `path`, resolving each alias to the actual
/// font family name found in the referenced TTF. font filenames are resolved
/// relative to `path`'s parent directory, matching mapfile conventions.
pub(crate) fn load(path: &Path) -> FontAliases {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "fontset file unreadable; aliases will resolve to themselves");
            return FontAliases::default();
        }
    };
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let mut map = HashMap::new();
    for (alias, filename) in parse_fontset_text(&src) {
        let font_path: PathBuf = base.join(&filename);
        match family_name_for_file(&font_path) {
            Some(family) => {
                map.insert(alias, family);
            }
            None => {
                warn!(
                    alias = %alias,
                    path = %font_path.display(),
                    "fontset entry could not be resolved; alias preserved verbatim",
                );
            }
        }
    }
    FontAliases { map }
}

/// build a `FontAliases` directly from in-memory pairs. test-only convenience
/// so unit tests can exercise the emit-time rewrite without touching disk.
#[cfg(test)]
pub(crate) fn from_pairs<I, S>(pairs: I) -> FontAliases
where
    I: IntoIterator<Item = (S, S)>,
    S: Into<String>,
{
    let mut map = HashMap::new();
    for (a, f) in pairs {
        map.insert(a.into(), f.into());
    }
    FontAliases { map }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
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
}
