#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn class_scale_window_gates_at_denom() {
    // half-open [25001, 100001): denom 25001 active, 100001 not.
    let s = ScaleWindow {
        min: Some(25_001),
        max: Some(100_001),
    };
    assert!(scale_window_contains(&s, 25_001));
    assert!(scale_window_contains(&s, 100_000));
    assert!(!scale_window_contains(&s, 25_000));
    assert!(!scale_window_contains(&s, 100_001));
}

#[test]
fn class_scale_window_open_bounds() {
    let s_no_min = ScaleWindow {
        min: None,
        max: Some(50),
    };
    assert!(scale_window_contains(&s_no_min, 0));
    assert!(scale_window_contains(&s_no_min, 49));
    assert!(!scale_window_contains(&s_no_min, 50));

    let s_no_max = ScaleWindow {
        min: Some(50),
        max: None,
    };
    assert!(!scale_window_contains(&s_no_max, 49));
    assert!(scale_window_contains(&s_no_max, 1_000_000));
}
