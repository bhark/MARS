//! per-request render-plan resolution.
//!
//! pure functions over [`RuntimeState`] and a request descriptor; no I/O, no
//! `await` points. the orchestration in `Runtime::render` (D4) calls these to
//! pick `(binding_id, level)` per layer and then enumerate the candidate
//! page-entry slice that intersects the request viewport.

use mars_config::{Layer, ScaleWindow};
use mars_types::{Bbox, BindingId, DecimationLevel, LevelMetadata, PageEntry};

use crate::RuntimeError;
use crate::state::RuntimeState;

/// fraction of a pixel below which a feature is not worth carrying through
/// the render pipeline. multiplied by the request's pixel-size-on-ground to
/// derive the "max acceptable `geometry_min_size_m`" cutoff during level
/// pick. K=0.5 is the LAZARUS Phase D starting point and is subject to
/// image-diff tuning in Phase E.
const PIXEL_SUBSAMPLE_K: f64 = 0.5;

/// OGC reference: 0.00028 m/pixel at 90 dpi (matches `denom_from_plan`).
const OGC_M_PER_PIXEL_AT_90DPI: f64 = 0.000_28;

/// pick the binding and decimation level for `layer` at `request_denom`.
///
/// returns `None` when no binding's `ScaleWindow` covers the request denom,
/// when the matching binding is absent from the manifest, or when the
/// binding has no levels at all (which the compiler never emits — the
/// fallback level-0 plan always produces at least one entry).
#[must_use]
pub fn pick_binding_and_level(
    layer: &Layer,
    request_denom: u32,
    state: &RuntimeState,
) -> Option<(BindingId, DecimationLevel)> {
    let pixel_size_m = pixel_size_at_denom(request_denom);
    for source in &layer.sources {
        if !scale_window_covers(source.scale.as_ref(), request_denom) {
            continue;
        }
        let id = match BindingId::try_new(source.from.as_str()) {
            Ok(id) => id,
            Err(_) => continue,
        };
        let Some(binding) = state.index.binding(&state.manifest, &id) else {
            continue;
        };
        let Some(level) = pick_level(&binding.levels, pixel_size_m) else {
            continue;
        };
        return Some((id, level));
    }
    None
}

/// enumerate the candidate page entries at `(binding_id, level)` whose
/// `spatial_bbox` intersects `viewport_native_crs`. the viewport is assumed
/// to already be expressed in the binding's native CRS; reprojection happens
/// in the caller (see [`reproject_viewport`]).
///
/// returns borrowed `&PageEntry` references into `state.manifest`. the
/// underlying slice is the contiguous run for `(binding_id, level)` recorded
/// in [`crate::PageIndex`]; the linear filter that follows is bounded by
/// that slice's length, not by the global `manifest.pages` size.
#[must_use]
pub fn resolve_pages<'m>(
    state: &'m RuntimeState,
    binding_id: &BindingId,
    level: DecimationLevel,
    viewport_native_crs: Bbox,
) -> Vec<&'m PageEntry> {
    state
        .index
        .page_slice(&state.manifest, binding_id, level)
        .iter()
        .filter(|p| bbox_intersects(p.spatial_bbox, viewport_native_crs))
        .collect()
}

/// reproject `viewport` from `from_crs` (the request CRS) into the binding's
/// native CRS. wraps the per-thread transformer cache in `mars-proj` and
/// surfaces the projection error as a [`RuntimeError`].
pub fn reproject_viewport(
    viewport: Bbox,
    from_crs: &mars_types::CrsCode,
    to_crs: &mars_types::CrsCode,
) -> Result<Bbox, RuntimeError> {
    if from_crs.as_str() == to_crs.as_str() {
        return Ok(viewport);
    }
    let xform = mars_proj::cached_transformer(from_crs, to_crs).map_err(map_proj_error)?;
    xform.transform_bbox(viewport).map_err(map_proj_error)
}

fn map_proj_error(e: mars_proj::ProjError) -> RuntimeError {
    RuntimeError::InvalidManifest {
        reason: format!("projection error: {e}"),
    }
}

fn pick_level(levels: &[LevelMetadata], pixel_size_m: f64) -> Option<DecimationLevel> {
    if levels.is_empty() {
        return None;
    }
    let threshold = pixel_size_m * PIXEL_SUBSAMPLE_K;
    // largest geometry_min_size_m still ≤ threshold; ties resolve to the higher
    // (more decimated) level number for determinism.
    let mut best: Option<&LevelMetadata> = None;
    for lm in levels {
        if lm.geometry_min_size_m.is_finite() && lm.geometry_min_size_m <= threshold {
            best = match best {
                None => Some(lm),
                Some(b)
                    if lm.geometry_min_size_m > b.geometry_min_size_m
                        || (lm.geometry_min_size_m == b.geometry_min_size_m && lm.level.get() > b.level.get()) =>
                {
                    Some(lm)
                }
                Some(b) => Some(b),
            };
        }
    }
    if let Some(b) = best {
        return Some(b.level);
    }
    // no level meets the cutoff → fall back to the finest level (smallest
    // geometry_min_size_m, breaking ties on lowest level number).
    levels
        .iter()
        .min_by(|a, b| {
            a.geometry_min_size_m
                .partial_cmp(&b.geometry_min_size_m)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.level.get().cmp(&b.level.get()))
        })
        .map(|lm| lm.level)
}

fn scale_window_covers(window: Option<&ScaleWindow>, denom: u32) -> bool {
    let Some(w) = window else { return true };
    let denom_u64 = u64::from(denom);
    if let Some(min) = w.min
        && denom_u64 < min
    {
        return false;
    }
    if let Some(max) = w.max
        && denom_u64 >= max
    {
        return false;
    }
    true
}

fn pixel_size_at_denom(denom: u32) -> f64 {
    f64::from(denom) * OGC_M_PER_PIXEL_AT_90DPI
}

fn bbox_intersects(a: Bbox, b: Bbox) -> bool {
    a.min_x <= b.max_x && a.max_x >= b.min_x && a.min_y <= b.max_y && a.max_y >= b.min_y
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::time::SystemTime;

    use mars_config::{Class, LabelSurvival, ScaleWindow, SourceBinding};
    use mars_style::Stylesheet;
    use mars_types::{
        BindingMetadata, ContentHash, CrsCode, HilbertKey, LayerId, MANIFEST_FORMAT_VERSION,
        Manifest, PageId, PageKey,
    };

    use super::*;

    fn level(level: u8, geometry_min_size_m: f64) -> LevelMetadata {
        LevelMetadata {
            level: DecimationLevel::new(level),
            vertex_tolerance_m: 0.0,
            geometry_min_size_m,
            label_min_priority: 0,
            page_count: 0,
            combined_bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
            hilbert_range_table: vec![],
        }
    }

    fn page(binding: &str, lvl: u8, hilbert_lo: u64, page_id: u64, bbox: Bbox) -> PageEntry {
        PageEntry {
            key: PageKey {
                binding_id: BindingId::try_new(binding).unwrap(),
                level: DecimationLevel::new(lvl),
                page_id: PageId::new(page_id),
            },
            content_hash: ContentHash::zero(),
            spatial_bbox: bbox,
            hilbert_range: (HilbertKey::new(hilbert_lo), HilbertKey::new(hilbert_lo + 1)),
            feature_count: 0,
            size_bytes: 0,
        }
    }

    fn binding_meta(id: &str, levels: Vec<LevelMetadata>) -> BindingMetadata {
        BindingMetadata {
            binding_id: BindingId::try_new(id).unwrap(),
            source_table: id.to_owned(),
            native_crs: CrsCode::new("EPSG:25832"),
            feature_count_total: 0,
            levels,
            page_membership_sidecar: None,
        }
    }

    fn state_with(pages: Vec<PageEntry>, bindings: Vec<BindingMetadata>) -> RuntimeState {
        let manifest = Manifest {
            format_version: MANIFEST_FORMAT_VERSION,
            version: 1,
            service: "test".into(),
            created_at: SystemTime::UNIX_EPOCH,
            bindings,
            pages,
            class_sidecars: vec![],
            label_sidecars: vec![],
            style_artifact: None,
            source_version: None,
            epoch: 0,
        };
        let index = crate::PageIndex::build(&manifest).unwrap();
        RuntimeState {
            manifest,
            stylesheet: Stylesheet::default(),
            config: None,
            index,
        }
    }

    fn cfg_layer(name: &str, sources: Vec<SourceBinding>) -> Layer {
        Layer {
            name: LayerId::new(name),
            title: String::new(),
            abstract_: String::new(),
            kind: "polygon".into(),
            scale: None,
            group: None,
            enable_get_feature_info: false,
            bbox: None,
            sources,
            classes: Vec::<Class>::new(),
            label: None,
            label_survival: LabelSurvival::default(),
        }
    }

    fn cfg_source(from: &str, scale: Option<ScaleWindow>) -> SourceBinding {
        SourceBinding {
            scale,
            band: None,
            from: from.into(),
            geometry_column: "geom".into(),
            id_column: None,
            attributes: vec![],
            levels: None,
            page_size_target_bytes: None,
            reconcile_every_cycles: None,
            sidecar_size_warn_bytes: None,
            simplifier: None,
        }
    }

    #[test]
    fn pick_level_largest_under_threshold() {
        let levels = vec![level(0, 0.0), level(1, 5.0), level(2, 20.0), level(3, 100.0)];
        // pixel_size_m = denom × 0.00028; with denom = 357142, pixel ≈ 100m,
        // threshold = 50m → expect level 2 (20m).
        let chosen = pick_level(&levels, 100.0).unwrap();
        assert_eq!(chosen.get(), 2);
    }

    #[test]
    fn pick_level_finest_when_all_too_coarse() {
        let levels = vec![level(0, 1.0), level(1, 5.0)];
        // threshold = 0.05m; nothing fits → fall back to finest (level 0).
        let chosen = pick_level(&levels, 0.1).unwrap();
        assert_eq!(chosen.get(), 0);
    }

    #[test]
    fn pick_level_empty_returns_none() {
        assert!(pick_level(&[], 1.0).is_none());
    }

    #[test]
    fn scale_window_inclusive_min_exclusive_max() {
        let w = ScaleWindow {
            min: Some(1000),
            max: Some(5000),
        };
        assert!(!scale_window_covers(Some(&w), 999));
        assert!(scale_window_covers(Some(&w), 1000));
        assert!(scale_window_covers(Some(&w), 4999));
        assert!(!scale_window_covers(Some(&w), 5000));
    }

    #[test]
    fn scale_window_open_bounds() {
        let lo = ScaleWindow {
            min: Some(100),
            max: None,
        };
        assert!(!scale_window_covers(Some(&lo), 99));
        assert!(scale_window_covers(Some(&lo), 999_999));
        let hi = ScaleWindow {
            min: None,
            max: Some(100),
        };
        assert!(scale_window_covers(Some(&hi), 0));
        assert!(!scale_window_covers(Some(&hi), 100));
    }

    #[test]
    fn resolve_pages_filters_by_bbox() {
        let pages = vec![
            page("a", 0, 0, 1, Bbox::new(0.0, 0.0, 10.0, 10.0)),
            page("a", 0, 1, 2, Bbox::new(20.0, 0.0, 30.0, 10.0)),
            page("a", 0, 2, 3, Bbox::new(5.0, 5.0, 15.0, 15.0)),
        ];
        let bindings = vec![binding_meta("a", vec![level(0, 0.0)])];
        let state = state_with(pages, bindings);
        let viewport = Bbox::new(0.0, 0.0, 12.0, 12.0);
        let hits = resolve_pages(
            &state,
            &BindingId::try_new("a").unwrap(),
            DecimationLevel::new(0),
            viewport,
        );
        assert_eq!(hits.len(), 2);
        // page ids 1 and 3 intersect.
        let ids: Vec<u64> = hits.iter().map(|p| p.key.page_id.get()).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&3));
    }

    #[test]
    fn resolve_pages_empty_for_unknown_binding() {
        let state = state_with(vec![], vec![]);
        let hits = resolve_pages(
            &state,
            &BindingId::try_new("ghost").unwrap(),
            DecimationLevel::new(0),
            Bbox::new(0.0, 0.0, 1.0, 1.0),
        );
        assert!(hits.is_empty());
    }

    #[test]
    fn pick_binding_and_level_picks_first_covering_source() {
        let pages = vec![page("a", 0, 0, 1, Bbox::new(0.0, 0.0, 10.0, 10.0))];
        let bindings = vec![binding_meta("a", vec![level(0, 0.0)])];
        let state = state_with(pages, bindings);
        let layer = cfg_layer("layer-a", vec![cfg_source("a", None)]);
        let resolved = pick_binding_and_level(&layer, 1000, &state).unwrap();
        assert_eq!(resolved.0.as_str(), "a");
        assert_eq!(resolved.1.get(), 0);
    }

    #[test]
    fn pick_binding_and_level_skips_out_of_window_sources() {
        let pages = vec![
            page("hi", 0, 0, 1, Bbox::new(0.0, 0.0, 10.0, 10.0)),
            page("lo", 0, 0, 2, Bbox::new(0.0, 0.0, 10.0, 10.0)),
        ];
        let bindings = vec![
            binding_meta("hi", vec![level(0, 0.0)]),
            binding_meta("lo", vec![level(0, 0.0)]),
        ];
        let state = state_with(pages, bindings);
        let layer = cfg_layer(
            "layer-a",
            vec![
                cfg_source(
                    "hi",
                    Some(ScaleWindow {
                        min: None,
                        max: Some(2000),
                    }),
                ),
                cfg_source(
                    "lo",
                    Some(ScaleWindow {
                        min: Some(2000),
                        max: None,
                    }),
                ),
            ],
        );
        let at_high = pick_binding_and_level(&layer, 1000, &state).unwrap();
        assert_eq!(at_high.0.as_str(), "hi");
        let at_low = pick_binding_and_level(&layer, 5000, &state).unwrap();
        assert_eq!(at_low.0.as_str(), "lo");
    }

    #[test]
    fn pick_binding_and_level_none_when_no_binding_in_manifest() {
        let state = state_with(vec![], vec![]);
        let layer = cfg_layer("layer-a", vec![cfg_source("ghost", None)]);
        assert!(pick_binding_and_level(&layer, 1000, &state).is_none());
    }
}
