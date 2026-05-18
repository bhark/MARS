use super::truncate_message;

#[test]
fn ascii_under_limit_is_unchanged() {
    assert_eq!(truncate_message("hello", 80), "hello");
}

#[test]
fn ascii_exactly_at_limit_is_unchanged() {
    let s = "x".repeat(80);
    assert_eq!(truncate_message(&s, 80), s);
}

#[test]
fn ascii_over_limit_is_truncated_with_ellipsis() {
    let s = "x".repeat(100);
    let out = truncate_message(&s, 10);
    assert_eq!(out.chars().count(), 10);
    assert!(out.ends_with('…'));
    assert_eq!(out, format!("{}…", "x".repeat(9)));
}

#[test]
fn multibyte_chars_count_as_one_char() {
    // five 4-byte glyphs (20 bytes); fits in limit 5.
    let s = "🦀🦀🦀🦀🦀";
    assert_eq!(s.chars().count(), 5);
    assert_eq!(truncate_message(s, 5), s);
}

#[test]
fn multibyte_truncation_respects_char_boundaries() {
    let s = "🦀🦀🦀🦀🦀🦀🦀🦀🦀🦀"; // ten crabs
    let out = truncate_message(s, 3);
    assert_eq!(out.chars().count(), 3);
    assert_eq!(out, "🦀🦀…");
    assert!(std::str::from_utf8(out.as_bytes()).is_ok());
}

#[test]
fn max_chars_zero_on_nonempty_input_returns_just_ellipsis() {
    // truncation branch always pushes the ellipsis; with max_chars=0 the
    // take(0) yields nothing so the result is the bare ellipsis. The only
    // production caller passes 80, so this edge sits outside the contract.
    assert_eq!(truncate_message("anything", 0), "…");
}

#[test]
fn max_chars_zero_on_empty_input_is_empty() {
    assert!(truncate_message("", 0).is_empty());
}

#[test]
fn max_chars_one_returns_just_ellipsis_when_over() {
    assert_eq!(truncate_message("hello", 1), "…");
}

#[test]
fn max_chars_one_returns_input_when_already_fits() {
    assert_eq!(truncate_message("x", 1), "x");
}
