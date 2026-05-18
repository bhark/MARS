#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn mapfile_text_lowers_bracket_refs() {
    assert_eq!(mapfile_text_to_template("[name]"), "{name}");
    assert_eq!(mapfile_text_to_template("([name])"), "{name}");
    assert_eq!(
        mapfile_text_to_template("[short_name] - [city]"),
        "{short_name} - {city}"
    );
}

#[test]
fn mapfile_text_passes_unknown_forms_through() {
    // unmatched bracket: leave intact rather than emit a half-template.
    assert_eq!(mapfile_text_to_template("[unclosed"), "[unclosed");
    // empty brackets: not an ident, pass through.
    assert_eq!(mapfile_text_to_template("[]"), "[]");
    // function-call expression form stays verbatim (operator must
    // translate the surrounding call by hand).
    assert_eq!(
        mapfile_text_to_template("(tostring([col],\"%f\"))"),
        "tostring({col},\"%f\")"
    );
}
