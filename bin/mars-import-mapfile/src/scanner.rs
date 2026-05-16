//! line-based scanner over a MapServer mapfile.
//!
//! mapfile syntax recap (only what we care about):
//! - keywords are case-insensitive
//! - `#` starts a comment to end-of-line, except inside double-quoted strings
//! - blocks open with a keyword (MAP, LAYER, CLASS, STYLE, PROJECTION, METADATA,
//!   LEGEND, LABEL, FEATURE, OUTPUTFORMAT, SYMBOL, WEB, REFERENCE, QUERYMAP,
//!   SCALEBAR, JOIN, COMPOSITE, CLUSTER, GRID, VALIDATION, CONFIG, SCALETOKEN)
//!   and close with END. VALUES is a sub-block opener inside SCALETOKEN.

use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};

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
    "SCALETOKEN",
    "VALUES",
    "POINTS",
];

// per-block directive keyword registries used by the packed-directive splitter.
// each set mirrors the per-block `from_token` enums in `directive.rs`. when a
// line packs multiple directives ("FONT \"x\" SIZE 8 POSITION CC"), the
// scanner splits the args on any unquoted token that matches the enclosing
// block's set. quoted strings never trigger a split.
//
// blocks without a registry (METADATA, VALUES, PROJECTION, POINTS, ...) fall
// back to today's no-split behaviour - their bodies are positional or free-form
// key/value pairs where directive-keyword collisions would be a false positive.

const MAP_DIRECTIVES: &[&str] = &[
    "NAME",
    "TITLE",
    "LAYER",
    "SYMBOL",
    "METADATA",
    "FONTSET",
    "LEGEND",
    "PROJECTION",
    "OUTPUTFORMAT",
    "FEATURE",
    "JOIN",
    "COMPOSITE",
    "CLUSTER",
    "GRID",
    "VALIDATION",
];

const LAYER_DIRECTIVES: &[&str] = &[
    "NAME",
    "TITLE",
    "TYPE",
    "DATA",
    "FILTER",
    "CLASSITEM",
    "LABELITEM",
    "MINSCALEDENOM",
    "MAXSCALEDENOM",
    "PROCESSING",
    "SCALETOKEN",
    "CLASS",
    "LABEL",
    "GROUP",
    "STATUS",
    "METADATA",
];

const CLASS_DIRECTIVES: &[&str] = &["NAME", "MINSCALEDENOM", "MAXSCALEDENOM", "EXPRESSION", "STYLE", "LABEL"];

const STYLE_DIRECTIVES: &[&str] = &[
    "COLOR",
    "OUTLINECOLOR",
    "WIDTH",
    "OUTLINEWIDTH",
    "PATTERN",
    "SYMBOL",
    "ANGLE",
    "SIZE",
    "OPACITY",
    "OFFSET",
    "GAP",
    "INITIALGAP",
    "LINEJOIN",
    "LINECAP",
    "GEOMTRANSFORM",
    "MINWIDTH",
    "MAXWIDTH",
];

const LABEL_DIRECTIVES: &[&str] = &[
    "TEXT",
    "FONT",
    "SIZE",
    "COLOR",
    "OUTLINECOLOR",
    "OUTLINEWIDTH",
    "PRIORITY",
    "MINDISTANCE",
    "REPEATDISTANCE",
    "MAXOVERLAPANGLE",
    "ANGLE",
    "POSITION",
    "OFFSET",
    "PARTIALS",
    "FORCE",
    "TYPE",
];

const SYMBOL_DIRECTIVES: &[&str] = &[
    "NAME",
    "TYPE",
    "ANGLE",
    "SIZE",
    "FILLED",
    "POINTS",
    "ANCHORPOINT",
    "FONT",
    "CHARACTER",
    "IMAGE",
];

/// directive keyword set for an enclosing block kind, or `None` if the block
/// is free-form (METADATA, VALUES, ...) and packed-directive splitting must
/// be disabled.
fn block_directives(kind: &str) -> Option<&'static [&'static str]> {
    let up = kind.to_ascii_uppercase();
    match up.as_str() {
        "MAP" => Some(MAP_DIRECTIVES),
        "LAYER" => Some(LAYER_DIRECTIVES),
        "CLASS" => Some(CLASS_DIRECTIVES),
        "STYLE" => Some(STYLE_DIRECTIVES),
        "LABEL" => Some(LABEL_DIRECTIVES),
        "SYMBOL" => Some(SYMBOL_DIRECTIVES),
        _ => None,
    }
}

fn matches_directive(set: &[&str], piece: &str) -> bool {
    set.iter().any(|d| d.eq_ignore_ascii_case(piece))
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ScanError {
    #[error("include cycle detected: {path:?}")]
    IncludeCycle { path: PathBuf },
    #[error("missing path in INCLUDE at line {line}")]
    MissingIncludePath { line: usize },
    #[error("cannot read include {path:?}: {source}")]
    ReadInclude {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

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

/// one whitespace-separated piece of a line, plus whether it originated from
/// a double-quoted string. quoted pieces never act as directive boundaries.
#[derive(Debug, Clone)]
struct Piece {
    value: String,
    quoted: bool,
}

/// tokenize a single line into whitespace-separated pieces, honouring quoted
/// strings. each piece carries a `quoted` flag so the packed-directive
/// splitter can ignore boundaries that came from inside `"..."`.
fn tokenize(line: &str) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let mut have = false;
    for ch in line.chars() {
        if in_str {
            if ch == '"' {
                in_str = false;
                out.push(Piece {
                    value: std::mem::take(&mut cur),
                    quoted: true,
                });
                have = false;
            } else {
                cur.push(ch);
            }
        } else if ch == '"' {
            if have {
                out.push(Piece {
                    value: std::mem::take(&mut cur),
                    quoted: false,
                });
                have = false;
            }
            in_str = true;
        } else if ch.is_whitespace() {
            if have {
                out.push(Piece {
                    value: std::mem::take(&mut cur),
                    quoted: false,
                });
                have = false;
            }
        } else {
            cur.push(ch);
            have = true;
        }
    }
    if have {
        out.push(Piece {
            value: cur,
            quoted: false,
        });
    } else if in_str {
        // unterminated quote: preserve previous behaviour where the partial
        // string still produced a token; treat as quoted.
        out.push(Piece {
            value: cur,
            quoted: true,
        });
    }
    out
}

/// running block-kind stack used while scanning. `None` entries represent
/// block kinds we have no directive registry for; depth still tracks but
/// packed-directive splitting is disabled at that level.
#[derive(Default)]
struct BlockStack {
    stack: Vec<Option<&'static str>>,
}

impl BlockStack {
    fn current(&self) -> Option<&'static str> {
        self.stack.last().copied().flatten()
    }

    fn push(&mut self, kind: &str) {
        let canon = canonical_block_kind(kind);
        self.stack.push(canon);
    }

    fn pop(&mut self) {
        self.stack.pop();
    }
}

fn canonical_block_kind(kw: &str) -> Option<&'static str> {
    let up = kw.to_ascii_uppercase();
    BLOCK_OPENERS.iter().find(|b| **b == up).copied()
}

/// scan source into a flat token stream, comments removed.
pub(crate) fn scan(src: &str) -> Vec<Token> {
    let mut toks = Vec::new();
    let mut stack = BlockStack::default();
    for (idx, raw) in src.lines().enumerate() {
        let line_no = idx + 1;
        let cleaned = strip_comment(raw).trim();
        if cleaned.is_empty() {
            continue;
        }
        let pieces = tokenize(cleaned);
        if pieces.is_empty() {
            continue;
        }
        emit_line(&pieces, line_no, &mut stack, &mut toks);
    }
    toks
}

/// flush the in-progress `(keyword, args)` as a `Token` and apply the block
/// stack update (push on opener, pop on END).
fn flush(keyword: &mut String, args: &mut Vec<String>, line_no: usize, stack: &mut BlockStack, toks: &mut Vec<Token>) {
    if keyword.is_empty() && args.is_empty() {
        return;
    }
    let kw = std::mem::take(keyword);
    let a = std::mem::take(args);
    if kw.eq_ignore_ascii_case("END") {
        stack.pop();
    } else if is_block_opener(&kw) && a.is_empty() {
        // dual-role: a block-opener keyword with args is a directive (e.g.
        // `SYMBOL "circle"` inside STYLE), not a block opener.
        stack.push(&kw);
    }
    toks.push(Token {
        line: line_no,
        keyword: kw,
        args: a,
    });
}

/// turn a tokenized line into one-or-more `Token`s, splitting on mid-line
/// END as well as on any unquoted piece that matches a directive keyword
/// for the enclosing block.
fn emit_line(pieces: &[Piece], line_no: usize, stack: &mut BlockStack, toks: &mut Vec<Token>) {
    let mut it = pieces.iter();
    let first = match it.next() {
        Some(p) => p,
        None => return,
    };
    let mut keyword = first.value.clone();
    let mut args: Vec<String> = Vec::new();
    for piece in it {
        // keyword-aware split: only if the enclosing block (after the pending
        // keyword would be flushed) has a directive registry, and only for
        // unquoted pieces that match an entry. checking the pending keyword
        // lets a packed opener like `LAYER NAME "x" TYPE LINE` split against
        // the LAYER registry even though LAYER has not yet been pushed onto
        // the stack. mid-line END is intentionally NOT split here - the
        // surrounding pipeline treats one-line blocks (`POINTS 1 1 END`) as
        // a single directive token with the END inside args.
        let enclosing = effective_enclosing(&keyword, stack);
        if !piece.quoted
            && let Some(set) = block_directives(enclosing.unwrap_or(""))
            && matches_directive(set, &piece.value)
        {
            flush(&mut keyword, &mut args, line_no, stack, toks);
            keyword = piece.value.clone();
            continue;
        }
        args.push(piece.value.clone());
    }
    flush(&mut keyword, &mut args, line_no, stack, toks);
}

/// the block kind that would enclose the *next* piece on the line, assuming
/// the pending `(keyword, args)` is flushed first. if the pending keyword is
/// itself a block opener, the next piece is inside that block.
fn effective_enclosing(pending: &str, stack: &BlockStack) -> Option<&'static str> {
    if let Some(opener) = canonical_block_kind(pending) {
        Some(opener)
    } else {
        stack.current()
    }
}

/// read a mapfile from disk and recursively inline INCLUDE directives.
pub(crate) fn scan_file(path: &Path) -> Result<Vec<Token>, ScanError> {
    let mut visited = HashSet::new();
    scan_file_recursive(path, &mut visited)
}

fn scan_file_recursive(path: &Path, visited: &mut HashSet<PathBuf>) -> Result<Vec<Token>, ScanError> {
    let canonical = path.canonicalize().map_err(|e| ScanError::ReadInclude {
        path: path.to_path_buf(),
        source: e,
    })?;
    if !visited.insert(canonical.clone()) {
        return Err(ScanError::IncludeCycle { path: canonical });
    }

    let src = std::fs::read_to_string(path).map_err(|e| ScanError::ReadInclude {
        path: path.to_path_buf(),
        source: e,
    })?;
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut out = Vec::new();

    for tok in scan(&src) {
        if tok.keyword.eq_ignore_ascii_case("INCLUDE") {
            let rel = tok
                .args
                .first()
                .ok_or(ScanError::MissingIncludePath { line: tok.line })?
                .trim_matches('"');
            let resolved = base_dir.join(rel);
            let included = scan_file_recursive(&resolved, visited)?;
            out.extend(included);
        } else {
            out.push(tok);
        }
    }

    visited.remove(&canonical);
    Ok(out)
}

/// find the matching END for the block whose opener is at `start`. returns the
/// inclusive range covering [opener .. END].
///
/// SYMBOL (and the other dual-role keywords) opens a block at MAP scope but
/// is a directive when used inside STYLE / CLASS scope with args
/// (`SYMBOL "arrow"`). A bare keyword with no args is the block-opener form;
/// args present means the line is a directive. Reading args here keeps the
/// depth counter accurate without needing scope context.
pub(crate) fn block_range(tokens: &[Token], start: usize) -> Option<Range<usize>> {
    let mut depth = 0usize;
    for (i, t) in tokens.iter().enumerate().skip(start) {
        let kw = t.keyword.to_ascii_uppercase();
        if is_block_opener(&kw) && t.args.is_empty() {
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

    fn values(pieces: Vec<Piece>) -> Vec<String> {
        pieces.into_iter().map(|p| p.value).collect()
    }

    #[test]
    fn strips_comments_outside_strings() {
        assert_eq!(strip_comment("NAME \"x\" # tail"), "NAME \"x\" ");
        assert_eq!(strip_comment("NAME \"a#b\""), "NAME \"a#b\"");
        assert_eq!(strip_comment("# whole line"), "");
    }

    #[test]
    fn tokenizes_quoted_strings() {
        assert_eq!(values(tokenize("NAME \"hello world\"")), vec!["NAME", "hello world"]);
        assert_eq!(values(tokenize("FOO bar baz")), vec!["FOO", "bar", "baz"]);
    }

    #[test]
    fn tokenize_marks_quoted_pieces() {
        let pieces = tokenize("FONT \"TYPE\" SIZE 8");
        assert_eq!(pieces.len(), 4);
        assert_eq!(pieces[0].value, "FONT");
        assert!(!pieces[0].quoted);
        assert_eq!(pieces[1].value, "TYPE");
        assert!(pieces[1].quoted, "quoted piece must be flagged");
        assert_eq!(pieces[2].value, "SIZE");
        assert!(!pieces[2].quoted);
        assert_eq!(pieces[3].value, "8");
        assert!(!pieces[3].quoted);
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

    #[test]
    fn scan_file_resolves_include() {
        let tmp = std::env::temp_dir().join("mars_import_scan_test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let main = tmp.join("main.map");
        let inc = tmp.join("inc.map");
        std::fs::write(&main, "MAP\n  NAME \"test\"\n  INCLUDE \"inc.map\"\nEND\n").unwrap();
        std::fs::write(&inc, "LAYER\n  NAME \"from_inc\"\nEND\n").unwrap();

        let toks = scan_file(&main).unwrap();
        let names: Vec<&str> = toks
            .iter()
            .filter(|t| t.keyword.eq_ignore_ascii_case("NAME"))
            .filter_map(|t| t.args.first().map(|s| s.as_str()))
            .collect();
        assert_eq!(names, vec!["test", "from_inc"]);
    }

    #[test]
    fn scan_file_detects_cycle() {
        let tmp = std::env::temp_dir().join("mars_import_cycle_test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let a = tmp.join("a.map");
        let b = tmp.join("b.map");
        std::fs::write(&a, "MAP\n  INCLUDE \"b.map\"\nEND\n").unwrap();
        std::fs::write(&b, "INCLUDE \"a.map\"\n").unwrap();

        let err = scan_file(&a).unwrap_err();
        assert!(
            matches!(err, ScanError::IncludeCycle { .. }),
            "expected cycle error, got {err}"
        );
    }

    #[test]
    fn scan_file_missing_include() {
        let tmp = std::env::temp_dir().join("mars_import_missing_test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let main = tmp.join("main.map");
        std::fs::write(&main, "MAP\n  INCLUDE \"nosuch.map\"\nEND\n").unwrap();

        let err = scan_file(&main).unwrap_err();
        assert!(
            matches!(err, ScanError::ReadInclude { .. }),
            "expected read error, got {err}"
        );
    }

    // packed-directive splitting

    #[test]
    fn packed_label_directives_split() {
        let src = "MAP\n  LAYER\n    LABEL\n      FONT \"arial\" TYPE truetype SIZE 8 POSITION CC PARTIALS false\n    END\n  END\nEND\n";
        let toks = scan(src);
        let inside: Vec<(&str, Vec<&str>)> = toks
            .iter()
            .filter(|t| {
                !matches!(
                    t.keyword.to_ascii_uppercase().as_str(),
                    "MAP" | "LAYER" | "LABEL" | "END"
                )
            })
            .map(|t| (t.keyword.as_str(), t.args.iter().map(String::as_str).collect()))
            .collect();
        assert_eq!(
            inside,
            vec![
                ("FONT", vec!["arial"]),
                ("TYPE", vec!["truetype"]),
                ("SIZE", vec!["8"]),
                ("POSITION", vec!["CC"]),
                ("PARTIALS", vec!["false"]),
            ]
        );
    }

    #[test]
    fn packed_style_directives_split() {
        let src = "MAP\n  LAYER\n    CLASS\n      STYLE\n        COLOR 1 2 3 WIDTH 0.5 OPACITY 80\n      END\n    END\n  END\nEND\n";
        let toks = scan(src);
        let inside: Vec<(&str, Vec<&str>)> = toks
            .iter()
            .filter(|t| matches!(t.keyword.to_ascii_uppercase().as_str(), "COLOR" | "WIDTH" | "OPACITY"))
            .map(|t| (t.keyword.as_str(), t.args.iter().map(String::as_str).collect()))
            .collect();
        assert_eq!(
            inside,
            vec![
                ("COLOR", vec!["1", "2", "3"]),
                ("WIDTH", vec!["0.5"]),
                ("OPACITY", vec!["80"]),
            ]
        );
    }

    #[test]
    fn quoted_directive_keyword_does_not_split() {
        // the quoted "SIZE" inside FONT args must not trigger a split even
        // though SIZE is a LABEL directive.
        let src = "MAP\n  LAYER\n    LABEL\n      FONT \"SIZE\" COLOR 0 0 0\n    END\n  END\nEND\n";
        let toks = scan(src);
        let label_body: Vec<(&str, Vec<&str>)> = toks
            .iter()
            .filter(|t| matches!(t.keyword.to_ascii_uppercase().as_str(), "FONT" | "COLOR"))
            .map(|t| (t.keyword.as_str(), t.args.iter().map(String::as_str).collect()))
            .collect();
        assert_eq!(
            label_body,
            vec![("FONT", vec!["SIZE"]), ("COLOR", vec!["0", "0", "0"]),]
        );
    }

    #[test]
    fn packed_split_respects_block_stack_for_class_under_layer() {
        // an inline LAYER opener with packed directives at MAP scope splits,
        // and subsequent directives inside the LAYER must split against the
        // LAYER registry (TYPE, DATA) - not MAP's.
        let src = "MAP NAME \"x\"\n  LAYER NAME \"l\" TYPE LINE\n  END\nEND\n";
        let toks = scan(src);
        let stream: Vec<(&str, Vec<&str>)> = toks
            .iter()
            .map(|t| (t.keyword.as_str(), t.args.iter().map(String::as_str).collect()))
            .collect();
        assert_eq!(
            stream,
            vec![
                ("MAP", vec![]),
                ("NAME", vec!["x"]),
                ("LAYER", vec![]),
                ("NAME", vec!["l"]),
                ("TYPE", vec!["LINE"]),
                ("END", vec![]),
                ("END", vec![]),
            ]
        );
    }

    // invariant: lines that don't pack directives must produce the exact
    // same token stream the pre-split scanner produced.

    #[test]
    fn invariant_non_packed_lines_unchanged() {
        // canonical one-directive-per-line shape used by all current fixtures.
        let src = r#"
MAP
  NAME "demo"
  LAYER
    NAME "roads"
    TYPE LINE
    DATA "geom FROM r"
    CLASS
      NAME "main"
      STYLE
        COLOR 1 2 3
        WIDTH 0.5
      END
      LABEL
        FONT "sans"
        SIZE 10
        COLOR 0 0 0
      END
    END
  END
END
"#;
        let toks = scan(src);
        let stream: Vec<(usize, &str, Vec<&str>)> = toks
            .iter()
            .map(|t| (t.line, t.keyword.as_str(), t.args.iter().map(String::as_str).collect()))
            .collect();
        assert_eq!(
            stream,
            vec![
                (2, "MAP", vec![]),
                (3, "NAME", vec!["demo"]),
                (4, "LAYER", vec![]),
                (5, "NAME", vec!["roads"]),
                (6, "TYPE", vec!["LINE"]),
                (7, "DATA", vec!["geom FROM r"]),
                (8, "CLASS", vec![]),
                (9, "NAME", vec!["main"]),
                (10, "STYLE", vec![]),
                (11, "COLOR", vec!["1", "2", "3"]),
                (12, "WIDTH", vec!["0.5"]),
                (13, "END", vec![]),
                (14, "LABEL", vec![]),
                (15, "FONT", vec!["sans"]),
                (16, "SIZE", vec!["10"]),
                (17, "COLOR", vec!["0", "0", "0"]),
                (18, "END", vec![]),
                (19, "END", vec![]),
                (20, "END", vec![]),
                (21, "END", vec![]),
            ]
        );
    }

    #[test]
    fn invariant_one_line_block_not_split() {
        // one-line blocks like `POINTS 1 1 END` are emitted as a single
        // directive token; the trailing END stays inside args. downstream
        // (`block_range`) uses the dual-role guard (`args.is_empty()`) so
        // depth accounting stays consistent.
        let src = "MAP\n  SYMBOL\n    NAME \"dot\"\n    POINTS 1 1 END\n  END\nEND\n";
        let toks = scan(src);
        let stream: Vec<(&str, Vec<&str>)> = toks
            .iter()
            .map(|t| (t.keyword.as_str(), t.args.iter().map(String::as_str).collect()))
            .collect();
        assert_eq!(
            stream,
            vec![
                ("MAP", vec![]),
                ("SYMBOL", vec![]),
                ("NAME", vec!["dot"]),
                ("POINTS", vec!["1", "1", "END"]),
                ("END", vec![]),
                ("END", vec![]),
            ]
        );
    }

    #[test]
    fn invariant_metadata_freeform_pairs_not_split() {
        // METADATA bodies are arbitrary key/value strings; a key happening to
        // equal a known directive name must NOT be split even when its value
        // follows another known-directive-shaped word.
        let src =
            "MAP\n  METADATA\n    \"NAME\" \"some service\"\n    \"TITLE\" \"a service titled NAME\"\n  END\nEND\n";
        let toks = scan(src);
        let stream: Vec<(&str, Vec<&str>)> = toks
            .iter()
            .map(|t| (t.keyword.as_str(), t.args.iter().map(String::as_str).collect()))
            .collect();
        assert_eq!(
            stream,
            vec![
                ("MAP", vec![]),
                ("METADATA", vec![]),
                ("NAME", vec!["some service"]),
                ("TITLE", vec!["a service titled NAME"]),
                ("END", vec![]),
                ("END", vec![]),
            ]
        );
    }
}
