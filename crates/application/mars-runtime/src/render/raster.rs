//! Raster layer render path. Resolves a `RasterLayerEntry` from the
//! manifest, fans out tile fetches through the registered `RasterSource`
//! adapter, decodes each tile to `DecodedImage`, and emits one
//! `DrawOp::Raster` per tile.
//!
//! Web-Mercator only for now. Plan CRS that disagrees with the source CRS,
//! or a non-`EPSG:3857` source CRS, both surface as a typed `NotImplemented`.

use std::sync::Arc;

use futures_util::StreamExt;
use mars_render_port::{DrawOp, PixelRect};
use mars_source::{RasterBinding, SourceError};
use mars_types::{LayerId, RasterLayerEntry};
use tracing::{Instrument, info_span};

use crate::decode::decode_to_rgba;
use crate::state::RuntimeState;
use crate::{Deps, RenderPlan, RuntimeError};

/// Half the Earth's circumference in metres at the equator (the canonical
/// Web Mercator world half-extent).
const WEB_MERCATOR_HALF_EXTENT_M: f64 = 20_037_508.342_789_244;

/// CRS code the first cut accepts for both `plan.crs` and
/// `binding.source_crs`. Anything else returns typed `NotImplemented`.
const SUPPORTED_CRS: &str = "EPSG:3857";

/// Build the list of `DrawOp::Raster` ops covering `plan` for one raster
/// layer entry. Tile fetches run concurrently, bounded by
/// `page_fetch_concurrency`. Returned ops are ordered `(y ascending, x
/// ascending)` so neighbouring-tile z-stacking is deterministic.
pub(crate) async fn render_raster_layer(
    state: &RuntimeState,
    deps: &Deps,
    plan: &RenderPlan,
    layer_id: &LayerId,
    page_fetch_concurrency: usize,
) -> Result<Vec<DrawOp>, RuntimeError> {
    let span = info_span!("render.layer.raster", layer = %layer_id);
    async move {
        let entry = lookup_entry(state, layer_id)?;
        ensure_supported_crs(&plan.crs, &entry.source_crs)?;

        let source =
            deps.raster_sources
                .get(&entry.collection)
                .ok_or_else(|| RuntimeError::RasterSourceNotRegistered {
                    collection: entry.collection.clone(),
                })?;

        let plan_m_per_pixel = plan.bbox.width() / f64::from(plan.width.max(1));
        let zoom = pick_zoom(plan_m_per_pixel, entry.tile_size, entry.max_level);
        let (x_min, x_max, y_min, y_max) = tile_range(plan.bbox, zoom, entry.tile_size);

        let binding = RasterBinding {
            collection: entry.collection.clone(),
            locator: entry.locator.clone(),
            source_crs: entry.source_crs.clone(),
            tile_size: entry.tile_size,
            max_level: entry.max_level,
        };

        let mut requests: Vec<(u32, u32)> = Vec::new();
        for y in y_min..=y_max {
            for x in x_min..=x_max {
                requests.push((x, y));
            }
        }

        let fetches = requests.into_iter().map(|(x, y)| {
            let source = source.clone();
            let binding = binding.clone();
            async move {
                // TileAbsent is a normal sparse-coverage signal; drop the slot
                // and continue. Any other source error is fatal for the page.
                match source.read_tile(&binding, zoom, x, y).await {
                    Ok(bytes) => {
                        let decoded = decode_to_rgba(&bytes.bytes, bytes.content_type).map_err(|e| {
                            RuntimeError::InvalidManifest {
                                reason: format!("raster tile z={zoom} x={x} y={y} decode: {e}"),
                            }
                        })?;
                        Ok::<_, RuntimeError>(Some((x, y, decoded)))
                    }
                    Err(SourceError::TileAbsent { .. }) => Ok(None),
                    Err(e) => Err(RuntimeError::RasterSource(e)),
                }
            }
        });
        let mut stream = futures_util::stream::iter(fetches).buffered(page_fetch_concurrency.max(1));

        let mut ordered: Vec<(u32, u32, mars_render_port::DecodedImage)> = Vec::new();
        while let Some(res) = stream.next().await {
            if let Some(tile) = res? {
                ordered.push(tile);
            }
        }
        // deterministic stacking: y ascending, then x ascending.
        ordered.sort_by_key(|(x, y, _)| (*y, *x));

        let plan_origin = MercatorPlanOrigin::for_plan(plan);
        let ops = ordered
            .into_iter()
            .map(|(x, y, decoded)| {
                let tile_bbox = tile_bbox_3857(zoom, x, y, entry.tile_size);
                let dst = plan_origin.tile_dst(tile_bbox);
                DrawOp::Raster {
                    tile: Arc::new(decoded),
                    dst,
                    opacity: entry.opacity,
                    blend_mode: None,
                }
            })
            .collect();
        Ok(ops)
    }
    .instrument(span)
    .await
}

fn lookup_entry<'s>(state: &'s RuntimeState, layer_id: &LayerId) -> Result<&'s RasterLayerEntry, RuntimeError> {
    state
        .manifest
        .raster_layers
        .iter()
        .find(|e| e.layer_id == *layer_id)
        .ok_or_else(|| RuntimeError::InvalidManifest {
            reason: format!("raster layer '{layer_id}' has no manifest entry"),
        })
}

fn ensure_supported_crs(plan_crs: &mars_types::CrsCode, source_crs: &mars_types::CrsCode) -> Result<(), RuntimeError> {
    if plan_crs.as_str() != SUPPORTED_CRS {
        return Err(RuntimeError::NotImplemented {
            what: "raster rendering: plan CRS must be EPSG:3857",
        });
    }
    if source_crs.as_str() != SUPPORTED_CRS {
        return Err(RuntimeError::NotImplemented {
            what: "raster rendering: source CRS must be EPSG:3857",
        });
    }
    Ok(())
}

/// Pick the zoom level whose tile resolution is at least the plan's
/// resolution (snap down so tiles are never blown up by fetch time). Clamps
/// to `[0, max_level]`.
pub(crate) fn pick_zoom(plan_m_per_pixel: f64, tile_size: u32, max_level: u32) -> u32 {
    if !plan_m_per_pixel.is_finite() || plan_m_per_pixel <= 0.0 {
        return 0;
    }
    let world_m = 2.0 * WEB_MERCATOR_HALF_EXTENT_M;
    // tile_res(z) >= plan_m_per_pixel  <=>  2^z <= world_m / (tile_size * plan_m_per_pixel)
    let ratio = world_m / (f64::from(tile_size) * plan_m_per_pixel);
    if !ratio.is_finite() || ratio <= 0.0 {
        return 0;
    }
    let z_float = ratio.log2().floor();
    if z_float.is_nan() || z_float < 0.0 {
        return 0;
    }
    let z = z_float as u32;
    z.min(max_level)
}

/// Tiles covering `bbox` (in Web-Mercator metres) at zoom `z`. Returns
/// `(x_min, x_max, y_min, y_max)`, inclusive on both ends, clamped to the
/// `[0, 2^z - 1]` axis range. y grows southward (slippy-map convention).
pub(crate) fn tile_range(bbox: mars_types::Bbox, z: u32, tile_size: u32) -> (u32, u32, u32, u32) {
    let world_m = 2.0 * WEB_MERCATOR_HALF_EXTENT_M;
    let tiles_per_side = 2u64.pow(z);
    let _ = tile_size; // size cancels in tile-index math; signature kept for symmetry
    let tile_extent = world_m / tiles_per_side as f64;
    let max_idx = (tiles_per_side as u32).saturating_sub(1);

    let x_min_f = (bbox.min_x + WEB_MERCATOR_HALF_EXTENT_M) / tile_extent;
    let x_max_f = (bbox.max_x + WEB_MERCATOR_HALF_EXTENT_M) / tile_extent;
    let y_min_f = (WEB_MERCATOR_HALF_EXTENT_M - bbox.max_y) / tile_extent;
    let y_max_f = (WEB_MERCATOR_HALF_EXTENT_M - bbox.min_y) / tile_extent;

    let x_min = clamp_index(x_min_f.floor(), max_idx);
    let x_max = clamp_index(x_max_f.floor(), max_idx);
    let y_min = clamp_index(y_min_f.floor(), max_idx);
    let y_max = clamp_index(y_max_f.floor(), max_idx);
    (x_min, x_max, y_min, y_max)
}

fn clamp_index(v: f64, max_idx: u32) -> u32 {
    if !v.is_finite() || v < 0.0 {
        return 0;
    }
    let as_u = v as u64;
    as_u.min(u64::from(max_idx)) as u32
}

/// Bbox of one tile in Web Mercator metres.
pub(crate) fn tile_bbox_3857(z: u32, x: u32, y: u32, tile_size: u32) -> mars_types::Bbox {
    let _ = tile_size; // size cancels in the bbox computation
    let world_m = 2.0 * WEB_MERCATOR_HALF_EXTENT_M;
    let tiles_per_side = 2u64.pow(z);
    let tile_extent = world_m / tiles_per_side as f64;
    let min_x = f64::from(x).mul_add(tile_extent, -WEB_MERCATOR_HALF_EXTENT_M);
    let max_y = WEB_MERCATOR_HALF_EXTENT_M - f64::from(y) * tile_extent;
    let max_x = min_x + tile_extent;
    let min_y = max_y - tile_extent;
    mars_types::Bbox::new(min_x, min_y, max_x, max_y)
}

/// Map plan-space metre coordinates to plan-space pixel coordinates.
struct MercatorPlanOrigin {
    plan_min_x: f64,
    plan_max_y: f64,
    px_per_m_x: f64,
    px_per_m_y: f64,
}

impl MercatorPlanOrigin {
    fn for_plan(plan: &RenderPlan) -> Self {
        let bw = plan.bbox.width();
        let bh = plan.bbox.height();
        Self {
            plan_min_x: plan.bbox.min_x,
            plan_max_y: plan.bbox.max_y,
            px_per_m_x: if bw > 0.0 { f64::from(plan.width) / bw } else { 0.0 },
            px_per_m_y: if bh > 0.0 { f64::from(plan.height) / bh } else { 0.0 },
        }
    }

    fn tile_dst(&self, tile_bbox: mars_types::Bbox) -> PixelRect {
        let px = (tile_bbox.min_x - self.plan_min_x) * self.px_per_m_x;
        // y axis flips: plan max_y maps to pixel y=0
        let py = (self.plan_max_y - tile_bbox.max_y) * self.px_per_m_y;
        let pw = tile_bbox.width() * self.px_per_m_x;
        let ph = tile_bbox.height() * self.px_per_m_y;
        PixelRect {
            x: px as f32,
            y: py as f32,
            w: pw as f32,
            h: ph as f32,
        }
    }
}

#[cfg(test)]
mod tests;
