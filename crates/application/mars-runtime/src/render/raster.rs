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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use bytes::Bytes;
    use mars_render_port::{Canvas, RenderError};
    use mars_source::{RasterBinding, RasterSource, SourceError, TileBytes};
    use mars_store::stub::{NotImplementedCache, NotImplementedStore};
    use mars_types::{Bbox, ImageFormat, Manifest, SourceCollectionId};

    use super::*;
    use crate::images::MutableImageRegistry;
    use crate::state::{PageIndex, RuntimeState};
    use crate::{Deps, RasterSourceRegistry};

    #[test]
    fn z0_full_world_picks_zoom_zero() {
        // at z=0 with tile_size=256, one tile spans the world (~156543 m/px).
        let plan_m_per_pixel = (2.0 * WEB_MERCATOR_HALF_EXTENT_M) / 256.0;
        assert_eq!(pick_zoom(plan_m_per_pixel, 256, 19), 0);
    }

    #[test]
    fn halving_resolution_doubles_zoom() {
        let z0_res = (2.0 * WEB_MERCATOR_HALF_EXTENT_M) / 256.0;
        // half the metres-per-pixel -> double the tile count per side -> z=1
        assert_eq!(pick_zoom(z0_res / 2.0, 256, 19), 1);
        assert_eq!(pick_zoom(z0_res / 4.0, 256, 19), 2);
        assert_eq!(pick_zoom(z0_res / 8.0, 256, 19), 3);
    }

    #[test]
    fn pick_zoom_clamps_to_max_level() {
        // very fine resolution clamps at max_level
        assert_eq!(pick_zoom(0.001, 256, 18), 18);
    }

    #[test]
    fn pick_zoom_floors_so_tiles_never_upsample_at_fetch() {
        let z0_res = (2.0 * WEB_MERCATOR_HALF_EXTENT_M) / 256.0;
        // a request slightly tighter than the z=0 ideal must stay at z=0,
        // never z=1, so tiles remain >= request resolution.
        assert_eq!(pick_zoom(z0_res * 0.99, 256, 19), 0);
    }

    #[test]
    fn pick_zoom_zero_for_invalid_input() {
        assert_eq!(pick_zoom(0.0, 256, 5), 0);
        assert_eq!(pick_zoom(-1.0, 256, 5), 0);
        assert_eq!(pick_zoom(f64::NAN, 256, 5), 0);
    }

    #[test]
    fn z0_whole_world_yields_one_tile() {
        let bbox = Bbox::new(
            -WEB_MERCATOR_HALF_EXTENT_M,
            -WEB_MERCATOR_HALF_EXTENT_M,
            WEB_MERCATOR_HALF_EXTENT_M,
            WEB_MERCATOR_HALF_EXTENT_M,
        );
        // floor of max gives index past the last tile, so we use max-eps tolerance:
        // even with floor() this returns (0, 0, 0, 0) on the boundary.
        let (x_min, x_max, y_min, y_max) = tile_range(bbox, 0, 256);
        assert_eq!((x_min, x_max, y_min, y_max), (0, 0, 0, 0));
    }

    #[test]
    fn z1_full_world_yields_four_tiles() {
        // inset slightly so floor() of the right/bottom edges stays inside.
        let half = WEB_MERCATOR_HALF_EXTENT_M - 1.0;
        let bbox = Bbox::new(-half, -half, half, half);
        let (x_min, x_max, y_min, y_max) = tile_range(bbox, 1, 256);
        assert_eq!((x_min, x_max, y_min, y_max), (0, 1, 0, 1));
    }

    #[test]
    fn tile_bbox_z0_is_world() {
        let b = tile_bbox_3857(0, 0, 0, 256);
        let h = WEB_MERCATOR_HALF_EXTENT_M;
        assert!((b.min_x + h).abs() < 1e-6);
        assert!((b.max_x - h).abs() < 1e-6);
        assert!((b.min_y + h).abs() < 1e-6);
        assert!((b.max_y - h).abs() < 1e-6);
    }

    #[test]
    fn tile_bbox_z1_quadrants_align() {
        let h = WEB_MERCATOR_HALF_EXTENT_M;
        let nw = tile_bbox_3857(1, 0, 0, 256);
        let ne = tile_bbox_3857(1, 1, 0, 256);
        let sw = tile_bbox_3857(1, 0, 1, 256);
        let se = tile_bbox_3857(1, 1, 1, 256);
        assert!((nw.min_x + h).abs() < 1e-6);
        assert!((nw.max_x).abs() < 1e-6);
        assert!((nw.max_y - h).abs() < 1e-6);
        assert!((ne.min_x).abs() < 1e-6);
        assert!((ne.max_x - h).abs() < 1e-6);
        assert!((sw.min_y + h).abs() < 1e-6);
        assert!((se.min_y + h).abs() < 1e-6);
    }

    // --- end-to-end orchestrator tests against a fake RasterSource -------

    fn encode_png_rgba(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(rgba).unwrap();
        drop(writer);
        out
    }

    #[derive(Debug)]
    struct FakeRasterSource {
        tile_size: u32,
        // each fetched tile is solid red; counts requests so concurrency-
        // bounded fan-out can be observed.
        calls: std::sync::Mutex<Vec<(u32, u32, u32)>>,
    }

    #[async_trait]
    impl RasterSource for FakeRasterSource {
        async fn read_tile(&self, _b: &RasterBinding, z: u32, x: u32, y: u32) -> Result<TileBytes, SourceError> {
            self.calls.lock().unwrap().push((z, x, y));
            let mut rgba = Vec::with_capacity((self.tile_size * self.tile_size * 4) as usize);
            for _ in 0..(self.tile_size * self.tile_size) {
                rgba.extend_from_slice(&[255, 0, 0, 255]);
            }
            let png = encode_png_rgba(self.tile_size, self.tile_size, &rgba);
            Ok(TileBytes {
                bytes: Bytes::from(png),
                content_type: "image/png",
            })
        }
    }

    #[derive(Debug)]
    struct UnusedRenderer;
    impl mars_render_port::Renderer for UnusedRenderer {
        fn render(&self, _canvas: Canvas, _ops: &[DrawOp]) -> Result<mars_render_port::Pixmap, RenderError> {
            Err(RenderError::NotImplemented { what: "test" })
        }
        fn measure_text(
            &self,
            _text: &str,
            _style: &mars_style::ResolvedLabelStyle,
        ) -> Result<mars_render_port::TextMetrics, RenderError> {
            Err(RenderError::NotImplemented { what: "test" })
        }
    }

    #[derive(Debug)]
    struct UnusedEncoder;
    impl mars_render_port::Encoder for UnusedEncoder {
        fn encode(
            &self,
            _pm: &mars_render_port::Pixmap,
            _fmt: ImageFormat,
        ) -> Result<Vec<u8>, mars_render_port::EncodeError> {
            Err(mars_render_port::EncodeError::NotImplemented { what: "test" })
        }
    }

    fn fake_deps(raster_sources: RasterSourceRegistry) -> Deps {
        Deps {
            store: Arc::new(NotImplementedStore),
            cache: Arc::new(NotImplementedCache),
            renderer: Arc::new(UnusedRenderer),
            encoder: Arc::new(UnusedEncoder),
            metrics: mars_observability::Metrics::new().unwrap(),
            fonts: Arc::new(crate::Fonts::with_default()),
            images: Arc::new(MutableImageRegistry::new()),
            raster_sources,
        }
    }

    fn state_with_raster_entry(entry: RasterLayerEntry) -> RuntimeState {
        let mut manifest = Manifest::empty(1, "test");
        manifest.raster_layers.push(entry);
        let index = PageIndex::build(&manifest).unwrap();
        RuntimeState {
            manifest,
            index,
            stylesheet: mars_style::Stylesheet::default(),
            config: None,
        }
    }

    fn webmercator_plan() -> RenderPlan {
        let h = WEB_MERCATOR_HALF_EXTENT_M;
        RenderPlan {
            layers: vec![LayerId::new("r")],
            bbox: Bbox::new(-h, -h, h, h),
            width: 256,
            height: 256,
            crs: mars_types::CrsCode::new("EPSG:3857"),
            format: ImageFormat::Png,
            scale_pixel_size_m: 0.000_28,
        }
    }

    fn raster_entry(layer: &str, collection: &str) -> RasterLayerEntry {
        RasterLayerEntry {
            layer_id: LayerId::new(layer),
            collection: SourceCollectionId::new(collection),
            locator: "https://example/{z}/{x}/{y}.png".into(),
            source_crs: mars_types::CrsCode::new("EPSG:3857"),
            tile_size: 256,
            max_level: 19,
            opacity: 1.0,
        }
    }

    #[tokio::test]
    async fn world_view_renders_single_z0_tile() {
        let layer = LayerId::new("r");
        let entry = raster_entry("r", "osm");
        let state = state_with_raster_entry(entry);
        let plan = webmercator_plan();
        let fake = Arc::new(FakeRasterSource {
            tile_size: 256,
            calls: std::sync::Mutex::new(Vec::new()),
        });
        let mut srcs = RasterSourceRegistry::new();
        srcs.insert(SourceCollectionId::new("osm"), fake.clone());
        let deps = fake_deps(srcs);

        let ops = render_raster_layer(&state, &deps, &plan, &layer, 4).await.unwrap();
        assert_eq!(ops.len(), 1, "world view at z=0 has exactly one tile");
        let calls = fake.calls.lock().unwrap();
        assert_eq!(*calls, vec![(0u32, 0u32, 0u32)]);
        match &ops[0] {
            DrawOp::Raster {
                tile,
                dst,
                opacity,
                blend_mode,
            } => {
                assert_eq!(tile.width, 256);
                assert_eq!(tile.height, 256);
                assert!((dst.w - 256.0).abs() < 1e-3);
                assert!((dst.h - 256.0).abs() < 1e-3);
                assert!((opacity - 1.0).abs() < f32::EPSILON);
                assert_eq!(*blend_mode, None);
            }
            other => panic!("expected DrawOp::Raster, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unregistered_collection_returns_typed_error() {
        let layer = LayerId::new("r");
        let entry = raster_entry("r", "osm");
        let state = state_with_raster_entry(entry);
        let plan = webmercator_plan();
        let deps = fake_deps(RasterSourceRegistry::new()); // empty registry
        let err = render_raster_layer(&state, &deps, &plan, &layer, 4)
            .await
            .expect_err("missing collection");
        assert!(matches!(err, RuntimeError::RasterSourceNotRegistered { collection } if collection.as_str() == "osm"));
    }

    #[tokio::test]
    async fn non_mercator_plan_crs_returns_not_implemented() {
        let layer = LayerId::new("r");
        let entry = raster_entry("r", "osm");
        let state = state_with_raster_entry(entry);
        let mut plan = webmercator_plan();
        plan.crs = mars_types::CrsCode::new("EPSG:4326");
        let fake: Arc<dyn RasterSource> = Arc::new(FakeRasterSource {
            tile_size: 256,
            calls: std::sync::Mutex::new(Vec::new()),
        });
        let mut srcs = RasterSourceRegistry::new();
        srcs.insert(SourceCollectionId::new("osm"), fake);
        let deps = fake_deps(srcs);
        let err = render_raster_layer(&state, &deps, &plan, &layer, 4)
            .await
            .expect_err("crs mismatch");
        assert!(matches!(err, RuntimeError::NotImplemented { what } if what.contains("CRS")));
    }

    #[derive(Debug)]
    struct AbsentRasterSource;

    #[async_trait]
    impl RasterSource for AbsentRasterSource {
        async fn read_tile(&self, _b: &RasterBinding, z: u32, x: u32, y: u32) -> Result<TileBytes, SourceError> {
            Err(SourceError::TileAbsent { z, x, y })
        }
    }

    #[tokio::test]
    async fn absent_tiles_are_skipped_not_propagated() {
        let layer = LayerId::new("r");
        let entry = raster_entry("r", "osm");
        let state = state_with_raster_entry(entry);
        let plan = webmercator_plan();
        let src: Arc<dyn RasterSource> = Arc::new(AbsentRasterSource);
        let mut srcs = RasterSourceRegistry::new();
        srcs.insert(SourceCollectionId::new("osm"), src);
        let deps = fake_deps(srcs);
        let ops = render_raster_layer(&state, &deps, &plan, &layer, 4).await.unwrap();
        assert!(ops.is_empty(), "absent tiles must be skipped, not produce ops");
    }

    #[tokio::test]
    async fn missing_manifest_entry_surfaces_invalid_manifest() {
        let layer = LayerId::new("missing");
        // state with no raster_layers
        let manifest = Manifest::empty(1, "test");
        let index = PageIndex::build(&manifest).unwrap();
        let state = RuntimeState {
            manifest,
            index,
            stylesheet: mars_style::Stylesheet::default(),
            config: None,
        };
        let plan = webmercator_plan();
        let deps = fake_deps(RasterSourceRegistry::new());
        let err = render_raster_layer(&state, &deps, &plan, &layer, 4)
            .await
            .expect_err("no manifest entry");
        assert!(matches!(err, RuntimeError::InvalidManifest { reason } if reason.contains("no manifest entry")));
    }

    #[test]
    fn plan_origin_maps_world_tile_to_full_canvas() {
        let h = WEB_MERCATOR_HALF_EXTENT_M;
        let plan = RenderPlan {
            layers: vec![],
            bbox: Bbox::new(-h, -h, h, h),
            width: 256,
            height: 256,
            crs: mars_types::CrsCode::new("EPSG:3857"),
            format: mars_types::ImageFormat::Png,
            scale_pixel_size_m: 0.000_28,
        };
        let origin = MercatorPlanOrigin::for_plan(&plan);
        let dst = origin.tile_dst(tile_bbox_3857(0, 0, 0, 256));
        // floating-point tolerance: dst must cover the canvas exactly.
        assert!((dst.x).abs() < 1e-3);
        assert!((dst.y).abs() < 1e-3);
        assert!((dst.w - 256.0).abs() < 1e-3);
        assert!((dst.h - 256.0).abs() < 1e-3);
    }
}
