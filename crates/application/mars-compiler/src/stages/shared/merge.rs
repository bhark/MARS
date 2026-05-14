//! merge a [`crate::render::RebuildOutcome`] into a prior manifest to
//! produce the next version. pure; cycle and rebalance both depend on it.

use mars_types::{BindingId, Manifest, PageEntry};

use crate::render;

/// Merge `outcome` into `prior` and stamp `next_version`. `source_version`
/// is propagated as-is; the cycle path threads through the latest batch's
/// version and the rebalance path forwards `prior.source_version`.
pub(crate) fn merge_manifest(
    prior: &Manifest,
    outcome: &render::RebuildOutcome,
    next_version: u64,
    source_version: Option<String>,
) -> Manifest {
    let replacement_page_keys: std::collections::HashSet<mars_types::PageKey> =
        outcome.replacement_pages.iter().map(|p| p.key.clone()).collect();
    let dropped_page_keys: std::collections::HashSet<mars_types::PageKey> =
        outcome.dropped_pages.iter().cloned().collect();

    let replacement_class_keys: std::collections::HashSet<(mars_types::LayerId, mars_types::PageKey)> = outcome
        .replacement_class_sidecars
        .iter()
        .map(|s| (s.layer_id.clone(), s.page_key.clone()))
        .collect();
    let replacement_label_keys: std::collections::HashSet<(mars_types::LayerId, mars_types::PageKey)> = outcome
        .replacement_label_sidecars
        .iter()
        .map(|s| (s.layer_id.clone(), s.page_key.clone()))
        .collect();
    let dropped_class_keys: std::collections::HashSet<(mars_types::LayerId, mars_types::PageKey)> =
        outcome.dropped_class_sidecars.iter().cloned().collect();
    let dropped_label_keys: std::collections::HashSet<(mars_types::LayerId, mars_types::PageKey)> =
        outcome.dropped_label_sidecars.iter().cloned().collect();

    // pages: keep prior pages whose key isn't replaced/dropped, then append
    // replacements.
    let mut pages: Vec<PageEntry> = prior
        .pages
        .iter()
        .filter(|p| !replacement_page_keys.contains(&p.key) && !dropped_page_keys.contains(&p.key))
        .cloned()
        .collect();
    pages.extend(outcome.replacement_pages.iter().cloned());
    pages.sort_by(|a, b| {
        a.key
            .binding_id
            .as_str()
            .cmp(b.key.binding_id.as_str())
            .then_with(|| a.key.level.cmp(&b.key.level))
            .then_with(|| a.hilbert_range.0.cmp(&b.hilbert_range.0))
    });

    // class / label sidecars: same shape.
    let mut class_sidecars = prior
        .class_sidecars
        .iter()
        .filter(|s| {
            let k = (s.layer_id.clone(), s.page_key.clone());
            !replacement_class_keys.contains(&k) && !dropped_class_keys.contains(&k)
        })
        .cloned()
        .collect::<Vec<_>>();
    class_sidecars.extend(outcome.replacement_class_sidecars.iter().cloned());

    let mut label_sidecars = prior
        .label_sidecars
        .iter()
        .filter(|s| {
            let k = (s.layer_id.clone(), s.page_key.clone());
            !replacement_label_keys.contains(&k) && !dropped_label_keys.contains(&k)
        })
        .cloned()
        .collect::<Vec<_>>();
    label_sidecars.extend(outcome.replacement_label_sidecars.iter().cloned());

    // bindings: replace touched ones, then refresh hilbert_range_table per
    // level via render::recompute_level_metadata.
    let refreshed_ids: std::collections::HashSet<BindingId> = outcome
        .refreshed_bindings
        .iter()
        .map(|b| b.binding_id.clone())
        .collect();
    let mut bindings: Vec<mars_types::BindingMetadata> = prior
        .bindings
        .iter()
        .filter(|b| !refreshed_ids.contains(&b.binding_id))
        .cloned()
        .collect();
    bindings.extend(outcome.refreshed_bindings.iter().cloned());
    for b in &mut bindings {
        for lm in &mut b.levels {
            *lm = render::recompute_level_metadata(lm, &pages, &b.binding_id);
        }
    }
    bindings.sort_by(|a, b| a.binding_id.as_str().cmp(b.binding_id.as_str()));

    Manifest {
        format_version: mars_types::MANIFEST_FORMAT_VERSION,
        version: next_version,
        service: prior.service.clone(),
        created_at: std::time::SystemTime::now(),
        bindings,
        pages,
        class_sidecars,
        label_sidecars,
        style_artifact: prior.style_artifact.clone(),
        image_artifact: prior.image_artifact.clone(),
        raster_layers: prior.raster_layers.clone(),
        source_version,
        epoch: next_version,
    }
}
