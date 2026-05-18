//! Render module: turns source rows into page artifacts and class / label
//! sidecars. Composed of focused submodules that share a small substrate
//! (the row primitive [`KeyedRow`] and the cycle output [`RebuildOutcome`]
//! / [`BindingOutput`]):
//!
//! - [`pass2`] streams the bound table once per binding and buckets rows
//!   into the planned (level, page_id) targets (bootstrap + truncate).
//! - [`incremental`] runs the per-binding dirty-set rebuild path and
//!   dispatches truncate vs. incremental on each cycle.
//! - [`rebalance`] applies Split / Merge ops by re-fetching affected
//!   features through the page-membership sidecar.
//! - [`flush`] encodes page artifacts + class / label sidecars and is
//!   shared by every entry point.
//! - [`keyed_row`] owns the shared row primitive and hydration helpers.
//! - [`metadata`] owns the small pure helpers (level metadata recompute,
//!   sidecar object-key, attr-value bridge).

mod flush;
mod incremental;
mod keyed_row;
mod metadata;
mod page_accumulator;
mod pass2;
mod rebalance;

pub use incremental::rebuild_pages;
pub use metadata::recompute_level_metadata;
pub use pass2::rebuild_binding_from_plan;
pub use rebalance::execute_rebalance;

pub(crate) use keyed_row::{
    KeyedRow, compute_row_fingerprint_from_wkb, drain_pruned_through, enforce_page_budget, hydrate_keyed_rows,
};
pub(crate) use metadata::{attr_value_to_artifact, empty_level_metadata, membership_sidecar_object_key};

use mars_types::{BindingMetadata, LayerSidecarEntry, PageEntry};

/// Output of one rebuild pass. Replaces dirty pages and refreshed bindings
/// in the prior manifest; pages and sidecars not listed here carry through
/// unchanged.
#[derive(Debug, Default)]
pub struct RebuildOutcome {
    /// Pages whose content was rewritten this cycle. Keyed by [`PageKey`]
    /// via [`PageEntry::key`]; callers replace any entry in the prior
    /// manifest with the same key.
    pub replacement_pages: Vec<PageEntry>,
    /// Pages that became empty after the rebuild and should be dropped
    /// from the manifest. A missing page is a missing page;
    /// no tombstones."
    pub dropped_pages: Vec<mars_types::PageKey>,
    /// Class sidecars rewritten this cycle.
    pub replacement_class_sidecars: Vec<LayerSidecarEntry>,
    /// Label sidecars rewritten this cycle.
    pub replacement_label_sidecars: Vec<LayerSidecarEntry>,
    /// Class sidecars dropped because their page is now empty.
    pub dropped_class_sidecars: Vec<(mars_types::LayerId, mars_types::PageKey)>,
    /// Label sidecars dropped because their page is now empty.
    pub dropped_label_sidecars: Vec<(mars_types::LayerId, mars_types::PageKey)>,
    /// Refreshed binding metadata (level table + new page-membership
    /// sidecar reference). One entry per binding touched by the cycle.
    pub refreshed_bindings: Vec<BindingMetadata>,
}

impl RebuildOutcome {
    /// Move every entry from `other` into `self`. Used by `rebuild_pages`
    /// to merge a per-binding local outcome into the shared one after the
    /// binding's rebuild succeeds; on failure the local is dropped instead.
    pub fn absorb(&mut self, mut other: RebuildOutcome) {
        self.replacement_pages.append(&mut other.replacement_pages);
        self.dropped_pages.append(&mut other.dropped_pages);
        self.replacement_class_sidecars
            .append(&mut other.replacement_class_sidecars);
        self.replacement_label_sidecars
            .append(&mut other.replacement_label_sidecars);
        self.dropped_class_sidecars.append(&mut other.dropped_class_sidecars);
        self.dropped_label_sidecars.append(&mut other.dropped_label_sidecars);
        self.refreshed_bindings.append(&mut other.refreshed_bindings);
    }
}

/// Output of one binding compile through the unified pipeline.
#[derive(Debug)]
pub struct BindingOutput {
    pub meta: BindingMetadata,
    pub pages: Vec<PageEntry>,
    pub class_sidecars: Vec<LayerSidecarEntry>,
    pub label_sidecars: Vec<LayerSidecarEntry>,
}
