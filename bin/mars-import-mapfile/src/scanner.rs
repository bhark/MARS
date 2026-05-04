//! line-based scanner over a MapServer mapfile.
//!
//! mapfile syntax recap (only what we care about):
//! - keywords are case-insensitive
//! - `#` starts a comment to end-of-line, except inside double-quoted strings
//! - blocks open with a keyword (MAP, LAYER, CLASS, STYLE, PROJECTION, METADATA,
//!   LEGEND, LABEL, FEATURE, OUTPUTFORMAT, SYMBOL, WEB, REFERENCE, QUERYMAP,
//!   SCALEBAR, JOIN, COMPOSITE, CLUSTER, GRID, VALIDATION, CONFIG) and close with END

use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Token {
    pub(crate) line: usize,
    pub(crate) keyword: String,
    pub(crate) args: Vec<String>,
}

const BLOCK_OPENERS: &[&str] = &[
    "MAP",
    "LAYER",
    "CLASS",
    "STYLE",
    "PROJECTION",
    "METADATA",
    "LEGEND",
    "LABEL",
    "FEATURE",
    "OUTPUTFORMAT",
    "SYMBOL",
    "WEB",
    "REFERENCE",
    "QUERYMAP",
    "SCALEBAR",
    "JOIN",
    "COMPOSITE",
    "CLUSTER",
    "GRID",
    "VALIDATION",
    "CONFIG",
];

pub(crate) fn is_block_opener(kw: &str) -> bool {
    let up = kw.to_ascii_uppercase();
    BLOCK_OPENERS.iter().any(|b| *b == up)
}

/// strip a `#` comment that lies outside any double-quoted string.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_str = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => in_str = !in_str,
            b'#' if !in_str => return &line[..i],
            _ => {}
        }
        i += 1;
    }
    line
}

/// tokenize a single line into whitespace-separated args, honouring quoted strings.
fn tokenize(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let mut have = false;
    for ch in line.chars() {
        if in_str {
            if ch == '"' {
                in_str = false;
                out.push(std::mem::take(&mut cur));
                have = false;
            } else {
                cur.push(ch);
            }
        } else if ch == '"' {
            if have {
                out.push(std::mem::take(&mut cur));
                have = false;
            }
            in_str = true;
        } else if ch.is_whitespace() {
            if have {
                out.push(std::mem::take(&mut cur));
                have = false;
            }
        } else {
            cur.push(ch);
            have = true;
        }
    }
    if have || in_str {
        out.push(cur);
    }
    out
}

/// scan source into a flat token stream, comments removed.
pub(crate) fn scan(src: &str) -> Vec<Token> {
    let mut toks = Vec::new();
    for (idx, raw) in src.lines().enumerate() {
        let line_no = idx + 1;
        let cleaned = strip_comment(raw).trim();
        if cleaned.is_empty() {
            continue;
        }
        let parts = tokenize(cleaned);
        if parts.is_empty() {
            continue;
        }
        let mut iter = parts.into_iter();
        let keyword = iter.next().unwrap_or_default();
        let args: Vec<String> = iter.collect();
        toks.push(Token {
            line: line_no,
            keyword,
            args,
        });
    }
    toks
}

/// find the matching END for the block whose opener is at `start`. returns the
/// inclusive range covering [opener .. END].
pub(crate) fn block_range(tokens: &[Token], start: usize) -> Option<Range<usize>> {
    let mut depth = 0usize;
    for (i, t) in tokens.iter().enumerate().skip(start) {
        let kw = t.keyword.to_ascii_uppercase();
        if is_block_opener(&kw) {
            depth += 1;
        } else if kw == "END" {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(start..i + 1);
            }
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn strips_comments_outside_strings() {
        assert_eq!(strip_comment("NAME \"x\" # tail"), "NAME \"x\" ");
        assert_eq!(strip_comment("NAME \"a#b\""), "NAME \"a#b\"");
        assert_eq!(strip_comment("# whole line"), "");
    }

    #[test]
    fn tokenizes_quoted_strings() {
        assert_eq!(tokenize("NAME \"hello world\""), vec!["NAME", "hello world"]);
        assert_eq!(tokenize("FOO bar baz"), vec!["FOO", "bar", "baz"]);
    }

    #[test]
    fn scans_balanced_block() {
        let src = "MAP\n  NAME \"t\"\n  LAYER\n    NAME \"l1\"\n  END\nEND\n";
        let toks = scan(src);
        let map_range = block_range(&toks, 0).expect("map block");
        assert_eq!(map_range.start, 0);
        assert_eq!(toks[map_range.end - 1].keyword.to_ascii_uppercase(), "END");
    }

    #[test]
    fn case_insensitive_openers() {
        assert!(is_block_opener("layer"));
        assert!(is_block_opener("LAYER"));
        assert!(is_block_opener("Class"));
        assert!(!is_block_opener("NAME"));
    }
}
