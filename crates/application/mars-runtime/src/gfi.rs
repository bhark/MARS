//! GetFeatureInfo (GFI): point hit-test that returns per-layer feature
//! attributes.
//!
//! the request shape mirrors a render plan (same bbox, width, height, crs),
//! plus a pixel-space point. for each configured layer with
//! `enable_get_feature_info`, we resolve the same `(binding, level)` the
//! render path would, expand the click into a 1-pixel bbox in the binding's
//! native CRS, query the page R-tree, and look up attribute rows by
//! feature_id via [`mars_artifact::ArtifactReader::attributes_by_feature_id`].
//!
//! the hit set uses the spatial index's bbox criterion. point-in-polygon
//! refinement against decoded geometry is a follow-up; for v1 the bbox-only
//! result is consistent with the pixel-buffer the click already represents.

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use mars_artifact::{ArtifactReader, AttrValue, SectionKind, SpatialIndex, decode_row};
use mars_config::Layer;
use mars_types::{Bbox, BindingMetadata, LayerId, PageEntry};

use crate::state::RuntimeState;
use crate::{Deps, RenderPlan, RuntimeError};
use crate::{fetch::fetch_page, plan as planning};

/// one feature hit from a GFI request, scoped to the originating layer.
#[derive(Debug, Clone)]
pub struct LayerFeatureInfo {
    /// layer the feature belongs to.
    pub layer: LayerId,
    /// stable feature identifier.
    pub feature_id: u64,
    /// decoded attribute values keyed by configured attribute name. order
    /// matches the on-disk row codec; callers that need a specific shape
    /// should materialise into their own map.
    pub attrs: Vec<(String, AttrValue)>,
}

/// resolve a pixel-space click into the matching `(layer, feature)` set.
///
/// `point_px` is in render-target pixel coordinates (origin at top-left,
/// `+x` right, `+y` down — matching the pixmap layout). out-of-bounds
/// coordinates produce an empty result.
pub(crate) async fn get_feature_info(
    state: &RuntimeState,
    deps: &Deps,
    plan: &RenderPlan,
    point_px: (u32, u32),
) -> Result<Vec<LayerFeatureInfo>, RuntimeError> {
    let config = state.config_or_err()?;
    let world = pixel_to_world(point_px, plan.bbox, plan.width, plan.height);
    let request_bbox = pixel_buffered_bbox(world, plan.bbox, plan.width, plan.height);

    let mut hits: Vec<LayerFeatureInfo> = Vec::new();
    for layer_id in &plan.layers {
        let layer_cfg = lookup_layer(config, layer_id)?;
        if !layer_cfg.enable_get_feature_info {
            continue;
        }
        let denom = crate::denom_from_plan(plan.bbox.width(), plan.width);
        let Some((binding_id, level)) = planning::pick_binding_and_level(layer_cfg, denom, state) else {
            continue;
        };
        let binding = state
            .index
            .binding(&state.manifest, &binding_id)
            .ok_or_else(|| RuntimeError::InvalidManifest {
                reason: format!("gfi: binding `{binding_id}` not in manifest"),
            })?;
        let native_bbox = planning::reproject_viewport(request_bbox, &plan.crs, &binding.native_crs)?;
        let pages = planning::resolve_pages(state, &binding_id, level, native_bbox);
        if pages.is_empty() {
            continue;
        }
        let layer_hits = collect_layer_hits(deps, layer_id, binding, &pages, native_bbox).await?;
        hits.extend(layer_hits);
    }
    Ok(hits)
}

async fn collect_layer_hits(
    deps: &Deps,
    layer_id: &LayerId,
    _binding: &BindingMetadata,
    pages: &[&PageEntry],
    native_bbox: Bbox,
) -> Result<Vec<LayerFeatureInfo>, RuntimeError> {
    let mut futs = FuturesUnordered::new();
    for page in pages {
        let store = deps.store.clone();
        let cache = deps.cache.clone();
        let entry = (*page).clone();
        futs.push(async move { fetch_page(&cache, &store, &entry).await.map(|b| (entry, b)) });
    }
    let mut out: Vec<LayerFeatureInfo> = Vec::new();
    while let Some(res) = futs.next().await {
        let (_entry, bytes) = res?;
        let mut page_hits = decode_page_hits(bytes, layer_id, native_bbox)?;
        out.append(&mut page_hits);
    }
    Ok(out)
}

fn decode_page_hits(
    bytes: Bytes,
    layer_id: &LayerId,
    native_bbox: Bbox,
) -> Result<Vec<LayerFeatureInfo>, RuntimeError> {
    let reader = ArtifactReader::open(bytes).map_err(map_artifact_err)?;
    let spatial_bytes = reader.section(SectionKind::SpatialIndex).map_err(map_artifact_err)?;
    let idx = SpatialIndex::open(spatial_bytes).map_err(map_artifact_err)?;
    if idx.is_empty() {
        return Ok(Vec::new());
    }
    let qbb = bbox_to_f32(native_bbox);
    let mut slots: Vec<u32> = Vec::new();
    idx.query(qbb, &mut slots);
    if slots.is_empty() {
        return Ok(Vec::new());
    }
    // walk the feature index once and collect feature_ids at the matched
    // slots. cheaper than decoding geometry — we only need the ids to drive
    // the attribute lookup.
    let geom_bytes = reader.section(SectionKind::GeometryPayload).map_err(map_artifact_err)?;
    let mut sorted = slots;
    sorted.sort_unstable();
    sorted.dedup();
    let iter = mars_artifact::iter_feature_index(&geom_bytes).map_err(map_artifact_err)?;
    let mut feature_ids: Vec<u64> = Vec::with_capacity(sorted.len());
    let mut cursor = 0usize;
    for (slot_idx, entry) in iter.enumerate() {
        let entry = entry.map_err(map_artifact_err)?;
        if cursor >= sorted.len() {
            break;
        }
        let want = sorted[cursor];
        let slot_u32 = u32::try_from(slot_idx).map_err(|_| RuntimeError::InvalidManifest {
            reason: "gfi: slot index overflow".into(),
        })?;
        if slot_u32 != want {
            continue;
        }
        cursor += 1;
        feature_ids.push(entry.id);
    }

    let mut out: Vec<LayerFeatureInfo> = Vec::with_capacity(feature_ids.len());
    for fid in feature_ids {
        match reader.attributes_by_feature_id(fid).map_err(map_artifact_err)? {
            Some(row_bytes) => {
                let attrs = decode_row(row_bytes).map_err(map_attr_err)?;
                out.push(LayerFeatureInfo {
                    layer: layer_id.clone(),
                    feature_id: fid,
                    attrs,
                });
            }
            None => {
                // attribute section missing the id: feature was indexed but
                // its row was not written. surface as a hit with no attrs.
                out.push(LayerFeatureInfo {
                    layer: layer_id.clone(),
                    feature_id: fid,
                    attrs: Vec::new(),
                });
            }
        }
    }
    Ok(out)
}

fn pixel_to_world(point_px: (u32, u32), viewport: Bbox, w: u32, h: u32) -> (f64, f64) {
    let dx = viewport.width();
    let dy = viewport.height();
    if w == 0 || h == 0 || !dx.is_finite() || !dy.is_finite() {
        return (viewport.min_x, viewport.min_y);
    }
    let nx = f64::from(point_px.0) / f64::from(w);
    let ny = f64::from(point_px.1) / f64::from(h);
    let world_x = viewport.min_x + nx * dx;
    let world_y = viewport.min_y + (1.0 - ny) * dy;
    (world_x, world_y)
}

fn pixel_buffered_bbox(world: (f64, f64), viewport: Bbox, w: u32, h: u32) -> Bbox {
    let pixel_size_x = if w == 0 { viewport.width() } else { viewport.width() / f64::from(w) };
    let pixel_size_y = if h == 0 { viewport.height() } else { viewport.height() / f64::from(h) };
    let hx = pixel_size_x * 0.5;
    let hy = pixel_size_y * 0.5;
    Bbox::new(world.0 - hx, world.1 - hy, world.0 + hx, world.1 + hy)
}

fn lookup_layer<'c>(config: &'c mars_config::Config, layer_id: &LayerId) -> Result<&'c Layer, RuntimeError> {
    config
        .layers
        .iter()
        .find(|l| l.name == *layer_id)
        .ok_or_else(|| RuntimeError::LayerNotDefined {
            layer: layer_id.as_str().to_owned(),
        })
}

fn bbox_to_f32(b: Bbox) -> [f32; 4] {
    [b.min_x as f32, b.min_y as f32, b.max_x as f32, b.max_y as f32]
}

fn map_artifact_err(e: mars_artifact::ArtifactError) -> RuntimeError {
    RuntimeError::InvalidManifest {
        reason: format!("gfi: artifact decode error: {e}"),
    }
}

fn map_attr_err(e: mars_artifact::AttrError) -> RuntimeError {
    RuntimeError::InvalidManifest {
        reason: format!("gfi: attribute decode error: {e}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn pixel_to_world_origin_matches_viewport_max_y() {
        let v = Bbox::new(0.0, 0.0, 100.0, 100.0);
        // (0, 0) pixel = top-left = (0, 100) world.
        let (x, y) = pixel_to_world((0, 0), v, 100, 100);
        assert!(x.abs() < 0.001);
        assert!((y - 100.0).abs() < 0.001);
    }

    #[test]
    fn pixel_to_world_far_corner_matches_viewport_min_y() {
        let v = Bbox::new(0.0, 0.0, 100.0, 100.0);
        // (100, 100) pixel = bottom-right = (100, 0) world.
        let (x, y) = pixel_to_world((100, 100), v, 100, 100);
        assert!((x - 100.0).abs() < 0.001);
        assert!(y.abs() < 0.001);
    }

    #[test]
    fn pixel_buffered_bbox_one_pixel_wide() {
        let v = Bbox::new(0.0, 0.0, 100.0, 100.0);
        let bb = pixel_buffered_bbox((50.0, 50.0), v, 100, 100);
        assert!((bb.width() - 1.0).abs() < 1e-6);
        assert!((bb.height() - 1.0).abs() < 1e-6);
    }
}
