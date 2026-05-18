//! Shared row substrate for the unified compile pipeline.
//!
//! One source row hydrates into a [`KeyedRow`]: a feature with attributes
//! preserved for class / label evaluation, the geometry-bytes estimate, a
//! hilbert key over the binding's combined bbox, and a stable per-row
//! fingerprint for tiebreaking. Used by pass-2 row routing
//! ([`super::pass2`]), the incremental rebuild path
//! ([`super::incremental`]), and the rebalance executor
//! ([`super::rebalance`]).
//!
//! `row_fingerprint` is BLAKE3 over WKB truncated to u64, used as the
//! final tiebreaker after `(hilbert_key, user_id)`. Within a
//! `(key, user_id, WKB)` tie attribute differences are NOT order-stable:
//! rows with identical geometry but different attrs hash to the same
//! fingerprint and are treated as equivalent for slot ordering. The
//! page-rebuild pipeline re-encodes attributes from the freshly hydrated
//! row regardless, so the substrate stays consistent.

use std::sync::Arc;

use futures_core::stream::BoxStream;
use futures_util::StreamExt;
use mars_artifact::{FeatureGeom, wkb_to_feature_geom};
use mars_source::{AttrValue, RowBytes, SourceError};
use mars_types::{Bbox, HilbertKey, PageId};

use crate::CompilerError;

#[derive(Debug, Clone)]
pub(crate) struct KeyedRow {
    pub(crate) feature: FeatureGeom,
    pub(crate) attrs: Arc<Vec<(String, AttrValue)>>,
    pub(crate) geom_bytes_estimate: u64,
    pub(crate) key: HilbertKey,
    pub(crate) row_fingerprint: u64,
}

/// Drain a row stream into deterministic-ordered [`KeyedRow`]s with hilbert
/// keys assigned over `combined_bbox`. Shared by the incremental, rebalance,
/// and (step 6) bootstrap-from-plan paths so all three hydrate rows
/// identically.
///
/// Memory budgets are enforced per-page by the caller (see
/// [`enforce_page_budget`]) - the hydration step itself is unbounded
/// because per-page guards catch single-page outliers and binding-wide
/// pressure is bounded by the feature-id set the caller assembles.
pub(crate) async fn hydrate_keyed_rows<'a>(
    mut stream: BoxStream<'a, Result<RowBytes, SourceError>>,
    combined_bbox: Bbox,
) -> Result<Vec<KeyedRow>, CompilerError> {
    let mut rows: Vec<KeyedRow> = Vec::new();
    while let Some(item) = stream.next().await {
        let row: RowBytes = item?;
        let geom_bytes_estimate = row.geometry.len() as u64;
        let row_fingerprint = compute_row_fingerprint_from_wkb(&row.geometry);
        let feature = wkb_to_feature_geom(&row.geometry, row.feature_id)?;
        let cx = (f64::from(feature.bbox[0]) + f64::from(feature.bbox[2])) / 2.0;
        let cy = (f64::from(feature.bbox[1]) + f64::from(feature.bbox[3])) / 2.0;
        let key = crate::hilbert::key_from_centroid(cx, cy, combined_bbox);
        rows.push(KeyedRow {
            feature,
            attrs: Arc::new(row.attributes),
            geom_bytes_estimate,
            key,
            row_fingerprint,
        });
    }
    Ok(rows)
}

/// Sum the working-set bytes of `rows` against `working_set_bytes`. Trips
/// [`CompilerError::ScratchBudgetExceeded`] with `Some(page_id)` when the
/// running total crosses the ceiling. Mirrors the per-row formula
/// [`hydrate_keyed_rows`] used to use, just measured per-page.
pub(crate) fn enforce_page_budget(
    rows: &[KeyedRow],
    working_set_bytes: u64,
    binding_id: &str,
    page_id: PageId,
) -> Result<(), CompilerError> {
    let mut observed: u64 = 0;
    for r in rows {
        let attr_bytes: u64 = r.attrs.iter().map(|(k, _)| (k.len() + 16) as u64).sum();
        let est = r.geom_bytes_estimate.saturating_add(attr_bytes).saturating_add(64);
        observed = observed.saturating_add(est);
        if observed > working_set_bytes {
            return Err(CompilerError::ScratchBudgetExceeded {
                binding: binding_id.to_string(),
                page_id: Some(page_id),
                observed_bytes: observed,
                budget_bytes: working_set_bytes,
            });
        }
    }
    Ok(())
}

/// Stable per-row tiebreaker. BLAKE3 over geometry bytes truncated to u64.
/// Identical WKB → identical fingerprint regardless of attribute payload;
/// attribute differences within a `(key, user_id, WKB)` tie are not
/// order-stable.
pub(crate) fn compute_row_fingerprint_from_wkb(wkb: &[u8]) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(wkb);
    let mut out = [0u8; 8];
    hasher.finalize_xof().fill(&mut out);
    u64::from_le_bytes(out)
}

/// Pull pruned rows whose hilbert key is `<= cap` off the head of the
/// pre-sorted slice, advancing `idx`.
pub(crate) fn drain_pruned_through<'a>(pruned: &'a [KeyedRow], idx: &mut usize, cap: HilbertKey) -> &'a [KeyedRow] {
    let start = *idx;
    while *idx < pruned.len() && pruned[*idx].key <= cap {
        *idx += 1;
    }
    &pruned[start..*idx]
}
