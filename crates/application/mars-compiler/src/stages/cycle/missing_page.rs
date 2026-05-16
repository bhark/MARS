//! cycle stage 3c: react to `IncrementalWarning::MissingPage` per the
//! affected binding's [`MissingPagePolicy`].
//!
//! a `MissingPage` warning means a change event's hilbert key fell outside
//! every page range for the binding - typically because the feature's
//! centroid sits outside the bootstrap `combined_bbox`. silently logging
//! and continuing leaves the row missing from the artifact until the next
//! reconcile cycle (up to `reconcile_every_cycles`, ~2h with defaults).
//!
//! per-binding policy resolution:
//! - `Warn`: log only; the next reconcile pass repairs drift.
//! - `Truncate`: escalate the binding to a full truncate-class rebuild
//!   this cycle. re-derives `combined_bbox` and re-emits every page.
//! - `Fail`: return [`CompilerError::MissingPageEscalation`] so the cycle
//!   aborts and operator alarms fire.
//!
//! runs after [`super::admission::enforce_ceiling`]: the ceiling already
//! caps total work; the missing-page pass only ever escalates further
//! (warn -> truncate -> fail). always-bumps the
//! `mars_compiler_missing_page_total{binding}` counter so operators see
//! the rate regardless of the resolved policy.

use std::collections::BTreeMap;

use mars_config::MissingPagePolicy;
use mars_observability::{Metrics, truncate_reason};
use mars_types::BindingId;

use crate::CompilerError;
use crate::incremental::{DirtyPages, IncrementalWarning};
use crate::plan::BootstrapPlan;

pub(crate) fn apply(dirty: &mut DirtyPages, plan: &BootstrapPlan, metrics: &Metrics) -> Result<(), CompilerError> {
    // group MissingPage warnings by binding so each binding hits at most
    // one policy lookup + at most one escalation action.
    let mut per_binding: BTreeMap<BindingId, Vec<&IncrementalWarning>> = BTreeMap::new();
    for w in &dirty.warnings {
        if let IncrementalWarning::MissingPage { binding_id, .. } = w {
            per_binding.entry(binding_id.clone()).or_default().push(w);
            metrics.inc_compiler_missing_page(binding_id.as_str());
        }
    }

    for (binding_id, warnings) in per_binding {
        let policy = plan
            .bindings
            .iter()
            .find(|b| b.binding_id == binding_id)
            .map(|b| b.missing_page_policy)
            .unwrap_or_default();

        match policy {
            MissingPagePolicy::Warn => {
                // logged already at cycle entry; nothing more to do here.
            }
            MissingPagePolicy::Truncate => {
                let bd = dirty.per_binding.entry(binding_id.clone()).or_default();
                if !bd.truncated {
                    tracing::warn!(
                        binding = binding_id.as_str(),
                        events = warnings.len(),
                        "missing-page escalation: truncating binding to re-derive combined_bbox"
                    );
                    metrics.inc_compiler_binding_truncate(binding_id.as_str(), truncate_reason::MISSING_PAGE);
                    bd.truncated = true;
                    bd.per_level.clear();
                    bd.observed.clear();
                }
            }
            MissingPagePolicy::Fail => {
                // group is non-empty by construction (we only insert when we
                // observe a MissingPage variant). pattern-match defensively
                // so a future warning variant added to this group can't
                // smuggle past as a silent no-op.
                for w in warnings {
                    if let IncrementalWarning::MissingPage { level, key, .. } = w {
                        return Err(CompilerError::MissingPageEscalation {
                            binding: binding_id.as_str().to_string(),
                            level: level.get(),
                            key: key.get(),
                        });
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
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
}
