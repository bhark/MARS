//! per-binding metadata aggregates.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::bbox::Bbox;
use crate::content::ArtifactEntry;
use crate::ids::{BindingId, CrsCode};
use crate::spatial::{DecimationLevel, HilbertKey, PageId};

/// per-decimation-level metadata on a binding. `hilbert_range_table`
/// duplicates the page-level Hilbert ranges in level-local sort order so
/// change-feed events can resolve `HilbertKey -> page` via a single binary
/// search without scanning the global `pages` vector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LevelMetadata {
    pub level: DecimationLevel,
    pub vertex_tolerance_m: f64,
    pub geometry_min_size_m: f64,
    pub label_min_priority: u32,
    pub page_count: u32,
    /// per-page `(hilbert_lo, hilbert_hi, page_id)` sorted ascending by
    /// `hilbert_lo`; binary-searchable. `page_id` is carried alongside the
    /// range because rebalance allocates fresh page ids that no longer
    /// match the table position; consumers must read `page_id` directly
    /// rather than reconstructing it from the array index.
    pub hilbert_range_table: Vec<(HilbertKey, HilbertKey, PageId)>,
}

/// per-binding metadata. one entry per `(table_or_view, geometry_column,
/// attribute_set, native_crs)` tuple in config; multi-table joined sources
/// are explicitly unsupported in v1 and are rejected at config-validation
/// time by `mars-config`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BindingMetadata {
    pub binding_id: BindingId,
    pub source_table: String,
    pub native_crs: CrsCode,
    pub feature_count_total: u64,
    /// hilbert-key basis for this binding. all levels share one bbox by
    /// construction; lives here so the type model can't drift.
    pub combined_bbox: Bbox,
    pub levels: Vec<LevelMetadata>,
    /// `(feature_id, hilbert_key)` sidecar pinned by the manifest commit.
    /// `None` when a binding runs in `REPLICA IDENTITY FULL` mode (old-row
    /// geometry comes from the change event itself, no sidecar needed).
    pub page_membership_sidecar: Option<ArtifactEntry>,
    /// incremental cycles elapsed since the last successful reconciliation
    /// pass. persisted so the cadence survives leader handover / process
    /// restart; hydrated into the compiler's in-memory cycle counter on
    /// startup and written back here each cycle.
    pub cycles_since_reconcile: u32,
    /// wall-clock time of the last successful reconciliation pass. drives
    /// the wall-clock floor in cadence selection: when set and older than
    /// the configured max age, the binding is forced into the next due set
    /// regardless of the cycle counter. `None` for never-reconciled bindings
    /// (typically right after bootstrap).
    pub last_reconcile_at: Option<SystemTime>,
}

#[cfg(test)]
mod tests;
