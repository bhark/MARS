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
/// pick.
const PIXEL_SUBSAMPLE_K: f64 = 0.5;

/// pick the binding and decimation level for `layer` at `request_denom`.
///
/// `m_per_pixel` is the standardised pixel size the caller used to derive
/// `request_denom` - typically [`crate::OGC_STANDARDIZED_PIXEL_SIZE_M`] or
/// the value implied by `service.scale_dpi`. Inverting it here keeps the
/// level-pick threshold consistent with the routing denom.
///
/// returns `None` when no binding's `ScaleWindow` covers the request denom,
/// when the matching binding is absent from the manifest, or when the
/// binding has no levels at all (which the compiler never emits - the
/// fallback level-0 plan always produces at least one entry).
#[must_use]
pub fn pick_binding_and_level(
    layer: &Layer,
    request_denom: u32,
    m_per_pixel: f64,
    state: &RuntimeState,
) -> Option<(BindingId, DecimationLevel)> {
    let pixel_size_m = pixel_size_at_denom(request_denom, m_per_pixel);
    for source in &layer.sources {
        if !scale_window_covers(source.scale.as_ref(), request_denom) {
            continue;
        }
        let Some(from) = (match &source.kind {
            mars_config::BindingKind::PostgisTable { from, .. } => Some(from.as_str()),
            mars_config::BindingKind::PostgisSql { .. } | mars_config::BindingKind::Vectorfile { .. } => None,
        }) else {
            // sql: / vectorfile bindings carry hash-derived ids the runtime
            // does not yet route at the manifest level. skip routing here so
            // the layer can still appear in capabilities while the snapshot
            // path catches up.
            continue;
        };
        let id = match BindingId::try_new(from) {
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

fn pixel_size_at_denom(denom: u32, m_per_pixel: f64) -> f64 {
    f64::from(denom) * m_per_pixel
}

fn bbox_intersects(a: Bbox, b: Bbox) -> bool {
    a.min_x <= b.max_x && a.max_x >= b.min_x && a.min_y <= b.max_y && a.max_y >= b.min_y
}

/// Compute the rendered image's denominator at the configured viewport.
/// Pure helper; exposed so the WMS / WMTS interface code can resolve
/// `<scaleHint>` style decisions without going through `Runtime`.
///
/// `m_per_pixel` is the standardised pixel size used to interpret the
/// denominator. Use [`crate::OGC_STANDARDIZED_PIXEL_SIZE_M`] for OGC-pure
/// behaviour; pass the value derived from `service.scale_dpi` for parity
/// with deployments that pin a different DPI (typically 96).
#[must_use]
pub fn denom_from_plan(bbox_width: f64, width_px: u32, m_per_pixel: f64) -> u32 {
    if !bbox_width.is_finite() || bbox_width <= 0.0 || width_px == 0 || !m_per_pixel.is_finite() || m_per_pixel <= 0.0 {
        return u32::MAX;
    }
    let denom = bbox_width / (f64::from(width_px) * m_per_pixel);
    if !denom.is_finite() || denom < 0.0 || denom > f64::from(u32::MAX) {
        u32::MAX
    } else {
        denom as u32
    }
}

#[cfg(test)]
mod tests;
