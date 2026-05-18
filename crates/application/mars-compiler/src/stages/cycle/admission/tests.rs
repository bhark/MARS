#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::collections::{BTreeMap, BTreeSet};

use mars_types::{BindingId, DecimationLevel, PageId};

use crate::incremental::{BindingDirty, DirtyPages};

fn dirty_pages(per_binding: Vec<(&str, BindingDirty)>) -> DirtyPages {
    let mut out = DirtyPages::default();
    for (id, bd) in per_binding {
        out.per_binding.insert(BindingId::try_new(id).unwrap(), bd);
    }
    out
}

fn binding_dirty_with_pages(level_pages: usize) -> BindingDirty {
    let mut per_level: BTreeMap<DecimationLevel, BTreeSet<PageId>> = BTreeMap::new();
    per_level.insert(
        DecimationLevel::new(0),
        (0..level_pages as u64).map(PageId::new).collect(),
    );
    BindingDirty {
        truncated: false,
        per_level,
        observed: BTreeSet::new(),
    }
}

#[test]
fn under_ceiling_left_alone() {
    let metrics = Metrics::new().unwrap();
    let mut dirty = dirty_pages(vec![("roads", binding_dirty_with_pages(5))]);
    enforce_ceiling(&mut dirty, Some(10), &metrics);
    let bd = dirty.per_binding.get(&BindingId::try_new("roads").unwrap()).unwrap();
    assert!(!bd.truncated);
    assert_eq!(bd.per_level.len(), 1);
}

#[test]
fn over_ceiling_escalates_to_truncate() {
    let metrics = Metrics::new().unwrap();
    let mut dirty = dirty_pages(vec![("roads", binding_dirty_with_pages(11))]);
    enforce_ceiling(&mut dirty, Some(10), &metrics);
    let bd = dirty.per_binding.get(&BindingId::try_new("roads").unwrap()).unwrap();
    assert!(bd.truncated);
    assert!(bd.per_level.is_empty());
    assert!(bd.observed.is_empty());
}

#[test]
fn one_binding_over_one_under_isolated() {
    let metrics = Metrics::new().unwrap();
    let mut dirty = dirty_pages(vec![
        ("roads", binding_dirty_with_pages(11)),
        ("buildings", binding_dirty_with_pages(5)),
    ]);
    enforce_ceiling(&mut dirty, Some(10), &metrics);
    assert!(
        dirty
            .per_binding
            .get(&BindingId::try_new("roads").unwrap())
            .unwrap()
            .truncated
    );
    assert!(
        !dirty
            .per_binding
            .get(&BindingId::try_new("buildings").unwrap())
            .unwrap()
            .truncated
    );
}

#[test]
fn already_truncated_skipped() {
    let metrics = Metrics::new().unwrap();
    let mut bd = binding_dirty_with_pages(11);
    bd.truncated = true;
    // pre-truncated bindings shouldn't fire the ceiling-driven metric.
    // (logic-wise we just keep `truncated=true` and never mutate.)
    let mut dirty = dirty_pages(vec![("roads", bd)]);
    enforce_ceiling(&mut dirty, Some(10), &metrics);
    let post = dirty.per_binding.get(&BindingId::try_new("roads").unwrap()).unwrap();
    assert!(post.truncated);
    assert_eq!(post.per_level.len(), 1, "we don't clobber prior truncation state");
}

#[test]
fn none_ceiling_is_a_noop() {
    let metrics = Metrics::new().unwrap();
    let mut dirty = dirty_pages(vec![("roads", binding_dirty_with_pages(1_000))]);
    enforce_ceiling(&mut dirty, None, &metrics);
    let bd = dirty.per_binding.get(&BindingId::try_new("roads").unwrap()).unwrap();
    assert!(!bd.truncated);
}
