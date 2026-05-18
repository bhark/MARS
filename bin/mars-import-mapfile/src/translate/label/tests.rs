#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn tok(keyword: &str, args: &[&str]) -> Token {
    Token {
        line: 1,
        keyword: keyword.into(),
        args: args.iter().map(|s| (*s).into()).collect(),
    }
}

#[test]
fn parse_label_angle_auto_sets_line_angle_mode_auto() {
    let p = parse_label(&[tok("ANGLE", &["AUTO"])]);
    assert!(p.unimplemented.is_empty());
    let line = p.placement_line.expect("line placement set");
    assert_eq!(line.angle_mode, Some(LineAngleMode::Auto));
}

#[test]
fn parse_label_angle_numeric_sets_angle_deg() {
    let p = parse_label(&[tok("ANGLE", &["45"])]);
    assert!(p.unimplemented.is_empty());
    assert!(p.placement_line.is_none(), "numeric ANGLE is not a line placement");
    assert_eq!(p.angle_deg, Some(45.0));
}

#[test]
fn parse_label_angle_follow_sets_line_angle_mode_follow() {
    let p = parse_label(&[tok("ANGLE", &["FOLLOW"])]);
    assert!(p.unimplemented.is_empty());
    let line = p.placement_line.expect("line placement set");
    assert_eq!(line.angle_mode, Some(LineAngleMode::Follow));
}

#[test]
fn parse_label_position_partials_offset_force_no_longer_flagged() {
    let p = parse_label(&[
        tok("POSITION", &["UC"]),
        tok("PARTIALS", &["TRUE"]),
        tok("OFFSET", &["2", "3"]),
        tok("FORCE", &["TRUE"]),
    ]);
    assert!(p.unimplemented.is_empty(), "got {:?}", p.unimplemented);
    assert_eq!(p.position, Some(AnchorPosition::Uc));
    assert_eq!(p.partials, Some(true));
    assert_eq!(p.offset_px, Some((2.0, 3.0)));
    assert_eq!(p.force, Some(true));
}

#[test]
fn parse_label_position_accepts_each_keyword() {
    for (kw, expected) in [
        ("UL", AnchorPosition::Ul),
        ("UC", AnchorPosition::Uc),
        ("UR", AnchorPosition::Ur),
        ("CL", AnchorPosition::Cl),
        ("CC", AnchorPosition::Cc),
        ("CR", AnchorPosition::Cr),
        ("LL", AnchorPosition::Ll),
        ("LC", AnchorPosition::Lc),
        ("LR", AnchorPosition::Lr),
        ("AUTO", AnchorPosition::Auto),
    ] {
        let p = parse_label(&[tok("POSITION", &[kw])]);
        assert_eq!(p.position, Some(expected), "POSITION {kw}");
    }
}

#[test]
fn parse_label_partials_accepts_truthy_and_falsy() {
    assert_eq!(parse_label(&[tok("PARTIALS", &["FALSE"])]).partials, Some(false));
    assert_eq!(parse_label(&[tok("PARTIALS", &["ON"])]).partials, Some(true));
    assert_eq!(parse_label(&[tok("PARTIALS", &["0"])]).partials, Some(false));
    // unknown values are ignored (no panic, no unimplemented entry).
    let p = parse_label(&[tok("PARTIALS", &["maybe"])]);
    assert!(p.partials.is_none());
}

#[test]
fn parse_label_offset_requires_two_args() {
    // one arg -> dropped silently.
    let p = parse_label(&[tok("OFFSET", &["3"])]);
    assert!(p.offset_px.is_none());
    // two non-numeric args -> dropped.
    let p = parse_label(&[tok("OFFSET", &["a", "b"])]);
    assert!(p.offset_px.is_none());
}

#[test]
fn parse_label_force_default_remains_unset() {
    let p = parse_label(&[]);
    assert!(p.force.is_none());
    assert!(p.partials.is_none());
    assert!(p.position.is_none());
}

#[test]
fn parse_label_only_type_bitmap_remains_unimplemented() {
    let p = parse_label(&[
        tok("POSITION", &["UC"]),
        tok("OFFSET", &["1", "2"]),
        tok("TYPE", &["BITMAP"]),
    ]);
    assert_eq!(
        p.unimplemented,
        vec!["LABEL.TYPE BITMAP"],
        "POSITION and OFFSET are now implemented; only TYPE BITMAP remains"
    );
}

#[test]
fn parse_label_flags_type_bitmap() {
    let p = parse_label(&[tok("TYPE", &["BITMAP"])]);
    assert_eq!(p.unimplemented, vec!["LABEL.TYPE BITMAP"]);
}

#[test]
fn parse_label_last_position_wins_on_repeat() {
    let p = parse_label(&[tok("POSITION", &["UC"]), tok("POSITION", &["UL"])]);
    assert!(p.unimplemented.is_empty());
    assert_eq!(p.position, Some(AnchorPosition::Ul));
}
