#![allow(clippy::expect_used, clippy::unwrap_used)]

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
    let src = "MAP\n  METADATA\n    \"NAME\" \"some service\"\n    \"TITLE\" \"a service titled NAME\"\n  END\nEND\n";
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
