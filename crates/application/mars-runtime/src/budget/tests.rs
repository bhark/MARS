#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mars_config::Render;

use super::*;

#[test]
fn explicit_config_wins() {
    let render = Render {
        pixel_budget: Some("256MiB".to_owned()),
        ..Render::default()
    };
    assert_eq!(resolve_pixel_budget(&render).unwrap(), (256 * 1024 * 1024) / 4);
}

#[test]
fn unset_resolves_to_at_least_the_floor() {
    // no explicit budget: the value depends on the test host's cgroup, but
    // it must always be a positive permit count at or above the floor.
    let budget = resolve_pixel_budget(&Render::default()).unwrap();
    assert!(budget >= MIN_PERMITS);
}

#[test]
fn cgroup_derivation_takes_40pct_after_reservation() {
    let limit = 4 * 1024 * 1024 * 1024;
    let after = limit - RESERVATION_BYTES;
    let expected = u32::try_from((u128::from(after) * 2 / 5) as u64 / 4).unwrap();
    assert_eq!(budget_from_limit(limit), expected);
}

#[test]
fn cgroup_derivation_floors_at_min() {
    // a limit at or below the reservation yields zero pixmap bytes -> floor.
    assert_eq!(budget_from_limit(RESERVATION_BYTES), MIN_PERMITS);
    assert_eq!(budget_from_limit(0), MIN_PERMITS);
}
