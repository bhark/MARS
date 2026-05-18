#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use mars_render_port::{Canvas, RenderError};
use mars_source::{RasterBinding, RasterSource, SourceError, TileBytes};
use mars_test_support::port_fakes::{NotImplementedCache, NotImplementedStore};
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
