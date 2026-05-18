#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use mars_types::{Bbox, CrsCode};

fn binding_id(s: &str) -> BindingId {
    BindingId::try_new(s).unwrap()
}

fn meta(id: &str, cycles: u32, last: Option<SystemTime>) -> BindingMetadata {
    BindingMetadata {
        binding_id: binding_id(id),
        source_table: id.into(),
        native_crs: CrsCode::new("EPSG:25832"),
        feature_count_total: 0,
        combined_bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
        levels: vec![],
        page_membership_sidecar: None,
        cycles_since_reconcile: cycles,
        last_reconcile_at: last,
    }
}

#[test]
fn hydrates_counter_from_prior_on_first_observation() {
    // simulates a leader handover: in-memory map empty, prior carries 23.
    let mut counters: HashMap<BindingId, u32> = HashMap::new();
    let prior = meta("roads", 23, None);
    // cadence 24, counter seeds to 23, +1 -> 24, due fires.
    assert!(step_counter(
        &mut counters,
        &binding_id("roads"),
        24,
        Some(&prior),
        None,
        SystemTime::UNIX_EPOCH,
    ));
    // counter reset to 0 after hit.
    assert_eq!(counters[&binding_id("roads")], 0);
}

#[test]
fn fresh_counter_with_no_prior_starts_at_one() {
    let mut counters: HashMap<BindingId, u32> = HashMap::new();
    // never reconciled, no prior. cadence 24, counter +1 -> 1, not due.
    assert!(!step_counter(
        &mut counters,
        &binding_id("roads"),
        24,
        None,
        None,
        SystemTime::UNIX_EPOCH,
    ));
    assert_eq!(counters[&binding_id("roads")], 1);
}

#[test]
fn wall_clock_floor_forces_due_when_last_reconcile_is_stale() {
    let mut counters: HashMap<BindingId, u32> = HashMap::new();
    let stale = SystemTime::UNIX_EPOCH;
    let now = stale + Duration::from_secs(7200); // 2h elapsed
    let prior = meta("roads", 0, Some(stale));
    // counter would say "not due" (1 < 24), wall-clock floor of 1h fires.
    assert!(step_counter(
        &mut counters,
        &binding_id("roads"),
        24,
        Some(&prior),
        Some(Duration::from_secs(3600)),
        now,
    ));
    assert_eq!(counters[&binding_id("roads")], 0);
}

#[test]
fn wall_clock_floor_quiet_when_within_max_age() {
    let mut counters: HashMap<BindingId, u32> = HashMap::new();
    let stale = SystemTime::UNIX_EPOCH;
    let now = stale + Duration::from_secs(60);
    let prior = meta("roads", 0, Some(stale));
    assert!(!step_counter(
        &mut counters,
        &binding_id("roads"),
        24,
        Some(&prior),
        Some(Duration::from_secs(3600)),
        now,
    ));
    assert_eq!(counters[&binding_id("roads")], 1);
}

#[test]
fn never_reconciled_binding_does_not_trigger_wall_clock_floor() {
    // last_reconcile_at = None: defer to counter, never force by age.
    let mut counters: HashMap<BindingId, u32> = HashMap::new();
    let prior = meta("roads", 5, None);
    assert!(!step_counter(
        &mut counters,
        &binding_id("roads"),
        24,
        Some(&prior),
        Some(Duration::from_secs(1)),
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
    ));
    assert_eq!(counters[&binding_id("roads")], 6);
}
