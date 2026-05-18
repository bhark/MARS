#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use std::collections::{BTreeMap, BTreeSet};

use mars_config::{SimplifierKind, SourceId};
use mars_types::{CrsCode, DecimationLevel, HilbertKey, PageId};

use crate::incremental::{BindingDirty, DirtyPages};
use crate::plan::{BindingPlan, BootstrapPlan, LevelPlan};

fn binding_plan(id: &str, policy: MissingPagePolicy) -> BindingPlan {
    BindingPlan {
        binding_id: BindingId::try_new(id).unwrap(),
        source_id: SourceId::new("default"),
        source_table: id.into(),
        filter: None,
        geometry_field: "geom".into(),
        id_field: Some("id".into()),
        attributes: vec![],
        native_crs: CrsCode::new("EPSG:25832"),
        levels: vec![LevelPlan {
            level: DecimationLevel::new(0),
            vertex_tolerance_m: 0.0,
            geometry_min_size_m: 0.0,
            label_min_priority: 0,
        }],
        page_size_target_bytes: 1024,
        sidecar_size_warn_bytes: u64::MAX,
        reconcile_every_cycles: 24,
        simplifier: SimplifierKind::Naive,
        missing_page_policy: policy,
        dsn: None,
    }
}

fn dirty_with_warning(binding: &str) -> DirtyPages {
    let mut d = DirtyPages::default();
    // mark some dirty pages so we can verify truncate clears them.
    let mut per_level: BTreeMap<DecimationLevel, BTreeSet<PageId>> = BTreeMap::new();
    per_level.insert(DecimationLevel::new(0), BTreeSet::from([PageId::new(0)]));
    d.per_binding.insert(
        BindingId::try_new(binding).unwrap(),
        BindingDirty {
            truncated: false,
            per_level,
            observed: BTreeSet::from([99]),
        },
    );
    d.warnings.push(IncrementalWarning::MissingPage {
        binding_id: BindingId::try_new(binding).unwrap(),
        level: DecimationLevel::new(0),
        key: HilbertKey::new(42),
    });
    d
}

#[test]
fn warn_policy_is_noop_on_dirty_set() {
    let plan = BootstrapPlan {
        bindings: vec![binding_plan("roads", MissingPagePolicy::Warn)],
        layers: vec![],
        raster_layers: vec![],
    };
    let mut d = dirty_with_warning("roads");
    let metrics = Metrics::new().unwrap();
    apply(&mut d, &plan, &metrics).unwrap();
    let bd = d.per_binding.get(&BindingId::try_new("roads").unwrap()).unwrap();
    assert!(!bd.truncated);
    assert_eq!(bd.per_level.len(), 1);
}

#[test]
fn truncate_policy_marks_binding_truncated() {
    let plan = BootstrapPlan {
        bindings: vec![binding_plan("roads", MissingPagePolicy::Truncate)],
        layers: vec![],
        raster_layers: vec![],
    };
    let mut d = dirty_with_warning("roads");
    let metrics = Metrics::new().unwrap();
    apply(&mut d, &plan, &metrics).unwrap();
    let bd = d.per_binding.get(&BindingId::try_new("roads").unwrap()).unwrap();
    assert!(bd.truncated);
    assert!(bd.per_level.is_empty());
    assert!(bd.observed.is_empty());
}

#[test]
fn fail_policy_returns_typed_error() {
    let plan = BootstrapPlan {
        bindings: vec![binding_plan("roads", MissingPagePolicy::Fail)],
        layers: vec![],
        raster_layers: vec![],
    };
    let mut d = dirty_with_warning("roads");
    let metrics = Metrics::new().unwrap();
    let err = apply(&mut d, &plan, &metrics).unwrap_err();
    assert!(matches!(err, CompilerError::MissingPageEscalation { .. }));
}

#[test]
fn no_missing_page_warning_is_noop() {
    let plan = BootstrapPlan {
        bindings: vec![binding_plan("roads", MissingPagePolicy::Truncate)],
        layers: vec![],
        raster_layers: vec![],
    };
    let mut d = DirtyPages::default();
    d.warnings.push(IncrementalWarning::MissingOldGeometry {
        binding_id: BindingId::try_new("roads").unwrap(),
        feature_id: 1,
    });
    let metrics = Metrics::new().unwrap();
    apply(&mut d, &plan, &metrics).unwrap();
    assert!(d.per_binding.is_empty());
}
