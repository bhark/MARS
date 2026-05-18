//! periodic page-membership sidecar reconciliation.
//!
//! between cycles the page-membership sidecar can drift from the source if a
//! change-feed message is dropped, the slot lags, or an admin edit slips
//! past the feed. this module compares the binding's source id set against
//! the sidecar's id set, then synthesises [`ChangeEvent`]s that repair the
//! drift on the next incremental cycle: orphans (in sidecar but not source)
//! emit `Delete`, missing rows (in source but not sidecar) fetch geometry
//! once and emit `Insert`.

use std::collections::BTreeMap;

use futures_util::StreamExt;
use mars_artifact::{wkb_centroid, wkb_to_feature_geom};
use mars_source::{ChangeEvent, GeometryEnvelope, SourceBinding as PortBinding, SourceCollectionId};
use mars_types::{Bbox, BindingId};

use crate::plan::BindingPlan;
use crate::sidecar::SidecarReader;
use crate::{CompilerError, Deps};

/// summary of one reconciliation pass over a binding.
///
/// Bag semantics: drift is counted per `user_id` rather than treated as a
/// boolean. `missing_in_sidecar[i] = (user_id, count)` says the source
/// returned `count` more rows for `user_id` than the sidecar carries; the
/// orphan map is the symmetric inverse. With non-unique source ids one
/// row can split into several substrate features, so a single user_id may
/// drift by more than one entry per cycle.
#[derive(Debug, Clone)]
pub struct ReconciliationReport {
    /// binding the pass ran against.
    pub binding_id: BindingId,
    /// `(user_id, count)` pairs for entries present in the source but not
    /// (yet) reflected in the sidecar.
    pub missing_in_sidecar: Vec<(u64, u32)>,
    /// `(user_id, count)` pairs for entries present in the sidecar but no
    /// longer present in the source.
    pub orphan_in_sidecar: Vec<(u64, u32)>,
}

/// reconcile output: report plus the synthetic events the caller should
/// feed into the next [`crate::incremental::IncrementalCycle`] to repair
/// drift.
#[derive(Debug, Clone)]
pub struct ReconciliationOutcome {
    /// drift summary. always populated, even when there is nothing to fix.
    pub report: ReconciliationReport,
    /// synthetic [`ChangeEvent`]s that close the drift. empty when in sync.
    pub synthetic_events: Vec<ChangeEvent>,
}

/// run one reconciliation pass over `binding_plan`'s source. streams all
/// source feature ids and diffs against `sidecar`. fetches geometry for
/// any missing ids so the synthetic events can carry envelopes.
pub async fn reconcile_binding(
    deps: &Deps,
    binding_plan: &BindingPlan,
    sidecar: &SidecarReader<'_>,
) -> Result<ReconciliationOutcome, CompilerError> {
    let port_binding = PortBinding::new(
        SourceCollectionId::new(binding_plan.binding_id.as_str()),
        binding_plan.source_table.clone(),
        binding_plan.geometry_field.clone(),
        binding_plan.id_field.as_deref().unwrap_or("id"),
        binding_plan.attributes.clone(),
        binding_plan.native_crs.clone(),
    )?
    .with_filter(binding_plan.filter.clone())
    .with_dsn(binding_plan.dsn.clone());

    // 1. count source rows per user_id (bag, not set - a non-unique source id
    //    contributes one count per row).
    let source = deps.source_for(binding_plan)?;
    let mut stream = source.stream_feature_ids(&port_binding).await?;
    let mut source_counts: BTreeMap<u64, u32> = BTreeMap::new();
    while let Some(item) = stream.next().await {
        let id = item?;
        if id < 0 {
            continue;
        }
        *source_counts.entry(id as u64).or_default() += 1;
    }

    // 2. count sidecar entries per user_id.
    let mut sidecar_counts: BTreeMap<u64, u32> = BTreeMap::new();
    for (id, _) in sidecar.iter() {
        *sidecar_counts.entry(id).or_default() += 1;
    }

    // 3. diff: every user_id with a non-zero delta drives synthetic events.
    //    union of keys so we can spot ids that left the source entirely.
    let mut all_ids: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    all_ids.extend(source_counts.keys().copied());
    all_ids.extend(sidecar_counts.keys().copied());

    let collection = SourceCollectionId::new(binding_plan.binding_id.as_str());
    let mut missing: Vec<(u64, u32)> = Vec::new();
    let mut orphan: Vec<(u64, u32)> = Vec::new();
    let mut to_fetch: Vec<u64> = Vec::new();
    let mut events: Vec<ChangeEvent> = Vec::new();

    for id in all_ids {
        let src = source_counts.get(&id).copied().unwrap_or(0);
        let side = sidecar_counts.get(&id).copied().unwrap_or(0);
        match src.cmp(&side) {
            std::cmp::Ordering::Greater => {
                missing.push((id, src - side));
                to_fetch.push(id);
            }
            std::cmp::Ordering::Less => {
                let diff = side - src;
                orphan.push((id, diff));
                // emit `diff` Delete events; with bag semantics we cannot
                // pin which specific multimap entry left, so the cycle
                // dirties every page covering any sidecar entry for this
                // user_id (incremental.rs::mark_old_side handles that via
                // sidecar.lookup_all).
                for _ in 0..diff {
                    events.push(ChangeEvent::Delete {
                        collection: collection.clone(),
                        feature_id: id,
                    });
                }
            }
            std::cmp::Ordering::Equal => {}
        }
    }

    // 4. missing: fetch geometry for every drifting user_id once and emit
    //    one synthetic Insert per returned row, each carrying its own
    //    envelope so the cycle can identify the dirty page via centroid.
    //    over-emitting beyond the strict deficit is benign - the rebuild
    //    sidecar refresh drops every multimap entry for an observed id
    //    and re-adds entries from the freshly fetched rows, which lands
    //    at parity regardless of the event count.
    if !to_fetch.is_empty() {
        let ids_signed: Vec<i64> = to_fetch
            .iter()
            .map(|id| i64::try_from(*id).unwrap_or(i64::MAX))
            .collect();
        let mut geom_stream = source.stream_rows_by_id(&port_binding, &ids_signed).await?;
        while let Some(item) = geom_stream.next().await {
            let row = item?;
            let envelope = envelope_from_wkb(&row.geometry, row.feature_id)?;
            events.push(ChangeEvent::Insert {
                collection: collection.clone(),
                feature_id: row.feature_id,
                new_envelope: envelope,
            });
        }
    }

    Ok(ReconciliationOutcome {
        report: ReconciliationReport {
            binding_id: binding_plan.binding_id.clone(),
            missing_in_sidecar: missing,
            orphan_in_sidecar: orphan,
        },
        synthetic_events: events,
    })
}

fn envelope_from_wkb(wkb: &[u8], feature_id: u64) -> Result<GeometryEnvelope, CompilerError> {
    let centroid = wkb_centroid(wkb)?;
    let feature = wkb_to_feature_geom(wkb, feature_id)?;
    Ok(GeometryEnvelope {
        centroid,
        bbox: Bbox::new(
            f64::from(feature.bbox[0]),
            f64::from(feature.bbox[1]),
            f64::from(feature.bbox[2]),
            f64::from(feature.bbox[3]),
        ),
    })
}

#[cfg(test)]
mod tests;
