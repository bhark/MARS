//! shared in-memory fixtures for runtime tests and benches.
//!
//! builds a minimal mars service end-to-end (config + manifest + page +
//! sidecars) backed by `mars-store::mem` stand-ins, so callers do not need
//! a real object store, cache, or compiler. all stand-ins are port-level;
//! no concrete adapter crate is referenced and the hexagonal-architecture
//! script stays green.
//!
//! gated on `feature = "test-fixtures"`. integration tests and benches
//! both opt in via `required-features` on their respective `[[test]]` /
//! `[[bench]]` Cargo declarations.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use mars_store::StoreError;
use mars_types::{ArtifactKey, ContentHash};

use mars_artifact::{
    ArtifactKind, ArtifactWriter, AttrValue, FeatureGeom, GeomKind, LabelCandidate, LabelShape, SpatialIndexBuilder,
    encode_row,
};
use mars_config::model::{
    ArtifactCache, ArtifactStore, Artifacts, Band, Cells, Class, ClassStyle, Compiler, Config, Interfaces, Layer,
    Observability, Render, Scales, ServiceMeta, Source, SourceBinding,
};
use mars_render_port::{Canvas, DrawOp, EncodeError, Encoder, ImageFormat, Pixmap, RenderError, Renderer, TextMetrics};
use mars_store::mem::{InMemoryCache, InMemoryStore};
use mars_store::{LocalCache, ObjectStore};
use mars_style::{Colour, FillPaint, LabelStyle, LabelSurvival, ResolvedLabelStyle, Style, Stylesheet};
use mars_types::{
    Bbox, BindingId, BindingMetadata, CrsCode, DecimationLevel, HilbertKey, ImageFormat as TImageFormat, LayerId,
    LayerSidecarEntry, LayerSidecarKind, MANIFEST_FORMAT_VERSION, Manifest, PageEntry, PageId, PageKey,
};

use crate::{Deps, Fonts, RenderPlan, Runtime, RuntimeState};

pub const REQUEST_CRS: &str = "EPSG:25832";

/// captures every DrawOp the runtime emits, then returns a 4×N×4 pixmap so
/// the encoder has something to encode. tests inspect the captured ops list
/// rather than pixel signatures.
#[derive(Default, Clone)]
pub struct CapturingRenderer {
    pub log: Arc<Mutex<Vec<DrawOp>>>,
}

impl Renderer for CapturingRenderer {
    fn render(&self, canvas: Canvas, ops: &[DrawOp]) -> Result<Pixmap, RenderError> {
        let mut log = self.log.lock().unwrap();
        log.extend(ops.iter().cloned());
        let n = canvas.width as usize * canvas.height as usize * 4;
        Ok(Pixmap {
            width: canvas.width,
            height: canvas.height,
            premultiplied_rgba: vec![0u8; n],
        })
    }

    fn measure_text(&self, text: &str, style: &ResolvedLabelStyle) -> Result<TextMetrics, RenderError> {
        // coarse stub matches the pre-Phase-F approximation so existing
        // layout assertions are stable.
        let chars = text.chars().count().max(1) as f32;
        let fs = style.font_size.max(1.0);
        Ok(TextMetrics {
            advance_x: chars * 0.55 * fs,
            ascent: fs * 0.8,
            descent: fs * 0.2,
        })
    }
}

/// returns a sentinel byte vec sized off the pixmap's dimensions; tests
/// don't inspect the encoded bytes.
#[derive(Default)]
pub struct StubEncoder;

impl Encoder for StubEncoder {
    fn encode(&self, pixmap: &Pixmap, _format: ImageFormat) -> Result<Vec<u8>, EncodeError> {
        Ok(vec![0u8; (pixmap.width * pixmap.height) as usize])
    }
}

/// the in-memory bits a test or bench needs handles to.
pub struct Fixture {
    pub runtime: Arc<Runtime>,
    pub config: Arc<Config>,
    pub render_log: Arc<Mutex<Vec<DrawOp>>>,
    pub store: Arc<dyn ObjectStore>,
    pub cache: Arc<dyn LocalCache>,
    pub manifest: Manifest,
    pub layer_id: LayerId,
    pub binding_id: BindingId,
    pub metrics: mars_observability::Metrics,
}

impl Fixture {
    /// a render plan covering the fixture viewport in the request CRS.
    pub fn render_plan(&self) -> RenderPlan {
        RenderPlan {
            layers: vec![self.layer_id.clone()],
            bbox: Bbox::new(0.0, 0.0, 100.0, 100.0),
            width: 64,
            height: 64,
            crs: CrsCode::new(REQUEST_CRS),
            format: TImageFormat::Png,
            scale_pixel_size_m: crate::OGC_STANDARDIZED_PIXEL_SIZE_M,
        }
    }
}

/// build the default fixture: one layer, one binding, one page with three
/// polygon features and a label per feature.
pub async fn build_fixture() -> Fixture {
    build_fixture_with(FixtureOptions::default()).await
}

#[derive(Clone)]
pub struct FixtureOptions {
    pub manifest_version: u64,
    pub feature_count: u64,
    /// label survival policy stamped onto the synthesised layer config.
    pub label_survival: LabelSurvival,
    /// extra label candidates whose `feature_idx` is out of range for the
    /// page's geometry section. used to exercise the FollowGeometry filter
    /// without having to manipulate viewport intersection.
    pub orphan_label_feature_idxs: Vec<u32>,
}

impl Default for FixtureOptions {
    fn default() -> Self {
        Self {
            manifest_version: 1,
            feature_count: 3,
            label_survival: LabelSurvival::Independent,
            orphan_label_feature_idxs: Vec::new(),
        }
    }
}

pub async fn build_fixture_with(opts: FixtureOptions) -> Fixture {
    let layer_id = LayerId::new("buildings");
    let binding_id = BindingId::try_new("public_buildings").unwrap();

    // synthesise per-feature geometry: a 10x10 square at (10*i, 10*i).
    let features: Vec<FeatureGeom> = (0..opts.feature_count)
        .map(|i| FeatureGeom {
            user_id: 1000 + i,
            bbox: [
                (i as f32) * 10.0,
                (i as f32) * 10.0,
                (i as f32) * 10.0 + 10.0,
                (i as f32) * 10.0 + 10.0,
            ],
            geom: GeomKind::Polygon(vec![vec![
                (f64::from(i as u32) * 10.0, f64::from(i as u32) * 10.0),
                (f64::from(i as u32) * 10.0 + 10.0, f64::from(i as u32) * 10.0),
                (f64::from(i as u32) * 10.0 + 10.0, f64::from(i as u32) * 10.0 + 10.0),
                (f64::from(i as u32) * 10.0, f64::from(i as u32) * 10.0 + 10.0),
                (f64::from(i as u32) * 10.0, f64::from(i as u32) * 10.0),
            ]]),
        })
        .collect();

    let page_bbox = Bbox::new(
        0.0,
        0.0,
        opts.feature_count as f64 * 10.0 + 10.0,
        opts.feature_count as f64 * 10.0 + 10.0,
    );

    // page artifact: spatial index + geometry payload + attributes.
    let mut spatial = SpatialIndexBuilder::new(mars_artifact::DEFAULT_NODE_SIZE).unwrap();
    for (slot, f) in features.iter().enumerate() {
        spatial.add(slot as u32, f.bbox);
    }
    let spatial_bytes = spatial.finish().unwrap();
    let attrs_pairs: Vec<(u32, Vec<u8>)> = features
        .iter()
        .enumerate()
        .map(|(slot, f)| {
            let pairs = vec![("name".to_string(), AttrValue::String(format!("feat-{}", f.user_id)))];
            (slot as u32, encode_row(&pairs).unwrap().to_vec())
        })
        .collect();

    let mut writer = ArtifactWriter::new(ArtifactKind::Source);
    writer
        .add_spatial_index(spatial_bytes)
        .add_geometry_payload(features.clone())
        .add_attributes(attrs_pairs)
        .set_bbox(page_bbox)
        .set_feature_count(opts.feature_count);
    let page_bytes = writer.finish().unwrap();
    let page_hash = mars_artifact::compute_content_hash(&page_bytes);

    // class sidecar: every slot → class index 0; one style ref pointing to
    // the stylesheet's "buildings__main" entry. label-style ref appended.
    let class_assignments: Vec<(u32, u16)> = (0..opts.feature_count).map(|i| (i as u32, 0u16)).collect();
    let style_refs: Vec<String> = vec!["buildings__main".to_string(), "buildings__label".to_string()];
    let mut writer = ArtifactWriter::new(ArtifactKind::Layer);
    writer
        .add_class_assignment(&class_assignments)
        .add_style_refs(&style_refs)
        .set_bbox(page_bbox);
    let class_bytes = writer.finish().unwrap();
    let class_hash = mars_artifact::compute_content_hash(&class_bytes);

    // label sidecar: one label per feature, point anchor at the feature
    // centroid, style_ref_idx = 1 (the appended label style ref).
    let mut labels: Vec<LabelCandidate> = features
        .iter()
        .enumerate()
        .map(|(slot, f)| LabelCandidate {
            feature_idx: Some(slot as u32),
            foreign_origin: false,
            priority: 100,
            style_ref_idx: 1,
            shape: LabelShape::Point {
                x: (f.bbox[0] + f.bbox[2]) * 0.5,
                y: (f.bbox[1] + f.bbox[3]) * 0.5,
            },
            text: format!("L{}", f.user_id),
        })
        .collect();
    // orphan labels: synthetic slot-bearing candidates whose feature_idx is
    // out of range for the page's geometry section. anchored inside the
    // page bbox so they survive canvas clipping; the only thing that
    // should drop them is the FollowGeometry filter on the runtime label
    // path.
    for &orphan_idx in &opts.orphan_label_feature_idxs {
        labels.push(LabelCandidate {
            feature_idx: Some(orphan_idx),
            foreign_origin: false,
            priority: 100,
            style_ref_idx: 1,
            shape: LabelShape::Point { x: 50.0, y: 50.0 },
            text: format!("ORPH{orphan_idx}"),
        });
    }
    // wire format requires ascending feature_idx for slot-bearing entries.
    labels.sort_by_key(|c| c.feature_idx.unwrap_or(u32::MAX));
    let mut writer = ArtifactWriter::new(ArtifactKind::Layer);
    writer.add_label_candidates(&labels).set_bbox(page_bbox);
    let label_bytes = writer.finish().unwrap();
    let label_hash = mars_artifact::compute_content_hash(&label_bytes);

    let page_key = PageKey {
        binding_id: binding_id.clone(),
        level: DecimationLevel::new(0),
        page_id: PageId::new(1),
    };
    let class_entry = LayerSidecarEntry {
        layer_id: layer_id.clone(),
        page_key: page_key.clone(),
        content_hash: class_hash,
        size_bytes: class_bytes.len() as u64,
        kind: LayerSidecarKind::Class,
    };
    let label_entry = LayerSidecarEntry {
        layer_id: layer_id.clone(),
        page_key: page_key.clone(),
        content_hash: label_hash,
        size_bytes: label_bytes.len() as u64,
        kind: LayerSidecarKind::Label,
    };
    let page_entry = PageEntry {
        key: page_key.clone(),
        content_hash: page_hash,
        spatial_bbox: page_bbox,
        hilbert_range: (HilbertKey::new(0), HilbertKey::new(u64::MAX)),
        feature_count: opts.feature_count,
        size_bytes: page_bytes.len() as u64,
    };

    let store: Arc<dyn ObjectStore> = Arc::new(InMemoryStore::new());
    store
        .put(&page_key.object_key(&page_hash).unwrap(), page_bytes)
        .await
        .unwrap();
    store
        .put(&class_entry.object_key().unwrap(), class_bytes)
        .await
        .unwrap();
    store
        .put(&label_entry.object_key().unwrap(), label_bytes)
        .await
        .unwrap();
    let cache: Arc<dyn LocalCache> = Arc::new(InMemoryCache::new());

    let binding_meta = BindingMetadata {
        binding_id: binding_id.clone(),
        source_table: "public.buildings".into(),
        native_crs: CrsCode::new(REQUEST_CRS),
        feature_count_total: opts.feature_count,
        combined_bbox: page_bbox,
        levels: vec![mars_types::LevelMetadata {
            level: DecimationLevel::new(0),
            vertex_tolerance_m: 0.0,
            geometry_min_size_m: 0.0,
            label_min_priority: 0,
            page_count: 1,
            hilbert_range_table: vec![(HilbertKey::new(0), HilbertKey::new(u64::MAX), PageId::new(1))],
        }],
        page_membership_sidecar: None,
        cycles_since_reconcile: 0,
        last_reconcile_at: None,
    };

    let manifest = Manifest {
        format_version: MANIFEST_FORMAT_VERSION,
        version: opts.manifest_version,
        service: "test-service".into(),
        created_at: SystemTime::UNIX_EPOCH,
        bindings: vec![binding_meta],
        pages: vec![page_entry],
        class_sidecars: vec![class_entry],
        label_sidecars: vec![label_entry],
        style_artifact: None,
        image_artifact: None,
        raster_layers: Vec::new(),
        source_version: None,
        epoch: 0,
    };

    let config = build_minimal_config(&layer_id, &binding_id, opts.label_survival);
    let stylesheet = build_minimal_stylesheet();
    let state = RuntimeState::from_config_and_manifest(&config, stylesheet, manifest.clone()).unwrap();

    let render_log = Arc::new(Mutex::new(Vec::<DrawOp>::new()));
    let renderer: Arc<dyn Renderer> = Arc::new(CapturingRenderer {
        log: render_log.clone(),
    });
    let encoder: Arc<dyn Encoder> = Arc::new(StubEncoder);
    let metrics = mars_observability::Metrics::new().unwrap();
    let fonts = Arc::new(Fonts::with_default());

    let deps = Deps {
        store: store.clone(),
        cache: cache.clone(),
        renderer,
        encoder,
        metrics: metrics.clone(),
        fonts,
        images: Arc::new(crate::images::MutableImageRegistry::new()),
        raster_sources: HashMap::new(),
    };

    let runtime = Arc::new(Runtime::from_state(Arc::new(state), deps));

    Fixture {
        runtime,
        config: Arc::new(config),
        render_log,
        store,
        cache,
        manifest,
        layer_id,
        binding_id,
        metrics,
    }
}

pub fn build_minimal_config(layer_id: &LayerId, binding_id: &BindingId, label_survival: LabelSurvival) -> Config {
    let mut size_per_band = BTreeMap::new();
    size_per_band.insert("hi".into(), "1024m".into());

    Config {
        service: ServiceMeta {
            name: "test".into(),
            ..Default::default()
        },
        sources: vec![Source {
            id: mars_config::SourceId::new("default"),
            native_crs: CrsCode::new(REQUEST_CRS),
            backend: mars_config::SourceBackend::Postgis(mars_config::PostgisBackend {
                dsn: "memory://".into(),
                change_feed: None,
                pool: Default::default(),
                bootstrap: None,
            }),
        }],
        artifacts: Artifacts {
            store: ArtifactStore {
                kind: "fs".into(),
                endpoint: None,
                bucket: None,
                prefix: None,
                path: Some("/tmp".into()),
                allow_http: false,
                ..Default::default()
            },
            cache: ArtifactCache {
                path: "/tmp".into(),
                max_size: "1GiB".into(),
                eviction: "lru".into(),
                trust_path_hash: false,
            },
        },
        scales: Scales {
            bands: vec![Band {
                name: "hi".into(),
                max_denom: 25_000,
            }],
        },
        cells: Cells {
            grid: "regular".into(),
            origin: [0.0, 0.0],
            size_per_band,
            extent: Some(Bbox::new(0.0, 0.0, 1000.0, 1000.0)),
        },
        interfaces: Interfaces::default(),
        tile_matrix_sets: Default::default(),
        reprojection: Default::default(),
        styles: Default::default(),
        layers: vec![Layer {
            name: layer_id.clone(),
            title: "Buildings".into(),
            abstract_: String::new(),
            kind: "polygon".into(),
            scale: None,
            group: None,
            bbox: None,
            sources: vec![SourceBinding {
                source: mars_config::SourceId::new("default"),
                scale: None,
                band: None,
                max_denom: None,
                filter: None,
                from: Some(binding_id.as_str().into()),
                sql: None,
                uri: None,
                format: None,
                source_crs: None,
                geometry_column: "geom".into(),
                id_column: None,
                attributes: vec!["name".into()],
                levels: None,
                page_size_target_bytes: None,
                reconcile_every_cycles: None,
                sidecar_size_warn_bytes: None,
                simplifier: None,
                on_missing_page: None,
            }],
            classes: vec![Class {
                name: "main".into(),
                title: String::new(),
                when: None,
                scale: None,
                style: ClassStyle::Inline(default_style()),
                label: None,
            }],
            label: None,
            label_survival,
            raster: None,
            wms: mars_config::LayerWms {
                enable_get_feature_info: true,
                ..Default::default()
            },
        }],
        observability: Observability::default(),
        render: Render::default(),
        compiler: Compiler::default(),
    }
}

pub fn build_minimal_stylesheet() -> Stylesheet {
    let mut ss = Stylesheet::default();
    ss.geometry
        .insert("buildings__main".into(), Arc::from(vec![default_style()]));
    ss.labels.insert(
        "buildings__label".into(),
        Arc::new(LabelStyle {
            font_family: "DejaVu Sans".into(),
            font_size: 12.0.into(),
            fill: Colour {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            },
            halo: None,
            priority: 100,
            min_distance: 0.0,
            position: mars_style::AnchorPosition::default(),
            offset_px: (0.0, 0.0),
            angle_deg: None,
            partials: true,
            force: false,
        }),
    );
    ss
}

pub fn default_style() -> Style {
    Style {
        fill: Some(FillPaint::Solid(Colour {
            r: 200,
            g: 200,
            b: 200,
            a: 255,
        })),
        stroke: Some(Colour {
            r: 64,
            g: 64,
            b: 64,
            a: 255,
        }),
        stroke_width: Some(1.0.into()),
        ..Default::default()
    }
}

// multi-layer fixture & sleeping-store decorator

/// wraps an `ObjectStore` and injects a per-key sleep on `get`. used in
/// integration tests to skew per-layer page-fetch completion order so the
/// FuturesUnordered reassembly step is forced to reorder.
pub struct SleepingStore {
    inner: Arc<dyn ObjectStore>,
    delays: HashMap<ArtifactKey, Duration>,
}

impl SleepingStore {
    pub fn new(inner: Arc<dyn ObjectStore>, delays: HashMap<ArtifactKey, Duration>) -> Self {
        Self { inner, delays }
    }
}

#[async_trait]
impl ObjectStore for SleepingStore {
    async fn get(&self, key: &ArtifactKey, expected: ContentHash) -> Result<Bytes, StoreError> {
        if let Some(d) = self.delays.get(key) {
            tokio::time::sleep(*d).await;
        }
        self.inner.get(key, expected).await
    }
    async fn put(&self, key: &ArtifactKey, body: Bytes) -> Result<ContentHash, StoreError> {
        self.inner.put(key, body).await
    }
    async fn delete(&self, key: &ArtifactKey) -> Result<(), StoreError> {
        self.inner.delete(key).await
    }
    async fn list(&self, prefix: &str) -> Result<Vec<ArtifactKey>, StoreError> {
        self.inner.list(prefix).await
    }
}

/// shape returned by `build_multi_layer_fixture`. each layer i has style
/// ref `layer_<i>__main`; the test uses these to verify draw-op order.
pub struct MultiLayerFixture {
    pub runtime: Arc<Runtime>,
    pub render_log: Arc<Mutex<Vec<DrawOp>>>,
    pub layer_ids: Vec<LayerId>,
    /// page-object keys per layer, in `layer_ids` order. tests can build
    /// per-layer delay maps off these.
    pub page_keys: Vec<ArtifactKey>,
}

impl MultiLayerFixture {
    pub fn render_plan(&self) -> RenderPlan {
        RenderPlan {
            layers: self.layer_ids.clone(),
            bbox: Bbox::new(0.0, 0.0, (self.layer_ids.len() as f64) * 10.0 + 10.0, 100.0),
            width: 64,
            height: 64,
            crs: CrsCode::new(REQUEST_CRS),
            format: TImageFormat::Png,
            scale_pixel_size_m: crate::OGC_STANDARDIZED_PIXEL_SIZE_M,
        }
    }
}

/// build N independent layers, each with its own binding, page, single
/// feature, and stylesheet entry `layer_<i>__main`. `store_decorator` wraps
/// the underlying `InMemoryStore` so callers can inject per-key delays via
/// `SleepingStore`.
pub async fn build_multi_layer_fixture<F>(n_layers: usize, store_decorator: F) -> MultiLayerFixture
where
    F: FnOnce(Arc<dyn ObjectStore>, &[ArtifactKey]) -> Arc<dyn ObjectStore>,
{
    assert!(n_layers > 0);

    let inner_store: Arc<dyn ObjectStore> = Arc::new(InMemoryStore::new());
    let cache: Arc<dyn LocalCache> = Arc::new(InMemoryCache::new());

    let mut layer_ids: Vec<LayerId> = Vec::with_capacity(n_layers);
    let mut binding_ids: Vec<BindingId> = Vec::with_capacity(n_layers);
    let mut bindings_meta: Vec<BindingMetadata> = Vec::with_capacity(n_layers);
    let mut pages: Vec<PageEntry> = Vec::with_capacity(n_layers);
    let mut class_sidecars: Vec<LayerSidecarEntry> = Vec::with_capacity(n_layers);
    let mut page_object_keys: Vec<ArtifactKey> = Vec::with_capacity(n_layers);

    for i in 0..n_layers {
        let layer_id = LayerId::new(format!("layer_{i}"));
        let binding_id = BindingId::try_new(format!("binding_{i}")).unwrap();

        // each layer's feature occupies its own 10x10 cell. all cells together
        // span `n_layers * 10 + 10` along x; render plan covers that bbox.
        let xo = (i as f64) * 10.0;
        let bbox = [xo as f32, 0.0_f32, (xo + 10.0) as f32, 10.0_f32];
        let feature = FeatureGeom {
            user_id: 1000 + i as u64,
            bbox,
            geom: GeomKind::Polygon(vec![vec![
                (xo, 0.0),
                (xo + 10.0, 0.0),
                (xo + 10.0, 10.0),
                (xo, 10.0),
                (xo, 0.0),
            ]]),
        };
        let page_bbox = Bbox::new(xo, 0.0, xo + 10.0, 10.0);

        // page artifact
        let mut spatial = SpatialIndexBuilder::new(mars_artifact::DEFAULT_NODE_SIZE).unwrap();
        spatial.add(0u32, feature.bbox);
        let spatial_bytes = spatial.finish().unwrap();
        let attrs_pairs: Vec<(u32, Vec<u8>)> = vec![(
            0u32,
            encode_row(&[("name".to_string(), AttrValue::String(format!("feat-L{i}")))])
                .unwrap()
                .to_vec(),
        )];
        let mut writer = ArtifactWriter::new(ArtifactKind::Source);
        writer
            .add_spatial_index(spatial_bytes)
            .add_geometry_payload(vec![feature])
            .add_attributes(attrs_pairs)
            .set_bbox(page_bbox)
            .set_feature_count(1);
        let page_bytes = writer.finish().unwrap();
        let page_hash = mars_artifact::compute_content_hash(&page_bytes);

        // class sidecar: slot 0 -> class 0 -> stylesheet "layer_<i>__main"
        let style_refs = vec![format!("layer_{i}__main")];
        let mut writer = ArtifactWriter::new(ArtifactKind::Layer);
        writer
            .add_class_assignment(&[(0u32, 0u16)])
            .add_style_refs(&style_refs)
            .set_bbox(page_bbox);
        let class_bytes = writer.finish().unwrap();
        let class_hash = mars_artifact::compute_content_hash(&class_bytes);

        let page_key = PageKey {
            binding_id: binding_id.clone(),
            level: DecimationLevel::new(0),
            page_id: PageId::new(1),
        };
        let class_entry = LayerSidecarEntry {
            layer_id: layer_id.clone(),
            page_key: page_key.clone(),
            content_hash: class_hash,
            size_bytes: class_bytes.len() as u64,
            kind: LayerSidecarKind::Class,
        };
        let page_entry = PageEntry {
            key: page_key.clone(),
            content_hash: page_hash,
            spatial_bbox: page_bbox,
            hilbert_range: (HilbertKey::new(0), HilbertKey::new(u64::MAX)),
            feature_count: 1,
            size_bytes: page_bytes.len() as u64,
        };

        let page_obj_key = page_key.object_key(&page_hash).unwrap();
        inner_store.put(&page_obj_key, page_bytes).await.unwrap();
        inner_store
            .put(&class_entry.object_key().unwrap(), class_bytes)
            .await
            .unwrap();

        bindings_meta.push(BindingMetadata {
            binding_id: binding_id.clone(),
            source_table: format!("public.layer_{i}"),
            native_crs: CrsCode::new(REQUEST_CRS),
            feature_count_total: 1,
            combined_bbox: page_bbox,
            levels: vec![mars_types::LevelMetadata {
                level: DecimationLevel::new(0),
                vertex_tolerance_m: 0.0,
                geometry_min_size_m: 0.0,
                label_min_priority: 0,
                page_count: 1,
                hilbert_range_table: vec![(HilbertKey::new(0), HilbertKey::new(u64::MAX), PageId::new(1))],
            }],
            page_membership_sidecar: None,
            cycles_since_reconcile: 0,
            last_reconcile_at: None,
        });
        pages.push(page_entry);
        class_sidecars.push(class_entry);
        page_object_keys.push(page_obj_key);
        layer_ids.push(layer_id);
        binding_ids.push(binding_id);
    }

    // pages must satisfy the manifest sort invariant
    // (binding_id, level, hilbert_range.0). binding_ids generated from a
    // numeric loop ("binding_10" < "binding_2" lex) break that order at
    // n_layers >= 10, so sort once here.
    pages.sort_by(|a, b| {
        a.key
            .binding_id
            .cmp(&b.key.binding_id)
            .then(a.key.level.cmp(&b.key.level))
            .then(a.hilbert_range.0.cmp(&b.hilbert_range.0))
    });

    let manifest = Manifest {
        format_version: MANIFEST_FORMAT_VERSION,
        version: 1,
        service: "test-multi".into(),
        created_at: SystemTime::UNIX_EPOCH,
        bindings: bindings_meta,
        pages,
        class_sidecars,
        label_sidecars: Vec::new(),
        style_artifact: None,
        image_artifact: None,
        raster_layers: Vec::new(),
        source_version: None,
        epoch: 0,
    };

    let config = build_multi_layer_config(&layer_ids, &binding_ids);
    let stylesheet = build_multi_layer_stylesheet(n_layers);
    let state = RuntimeState::from_config_and_manifest(&config, stylesheet, manifest).unwrap();

    // hand the inner store + page keys to the decorator so callers can build
    // a delay map keyed off the actual object keys.
    let store: Arc<dyn ObjectStore> = store_decorator(inner_store, &page_object_keys);

    let render_log = Arc::new(Mutex::new(Vec::<DrawOp>::new()));
    let renderer: Arc<dyn Renderer> = Arc::new(CapturingRenderer {
        log: render_log.clone(),
    });
    let encoder: Arc<dyn Encoder> = Arc::new(StubEncoder);
    let metrics = mars_observability::Metrics::new().unwrap();
    let fonts = Arc::new(Fonts::with_default());
    let deps = Deps {
        store,
        cache,
        renderer,
        encoder,
        metrics,
        fonts,
        images: Arc::new(crate::images::MutableImageRegistry::new()),
        raster_sources: HashMap::new(),
    };
    let runtime = Arc::new(Runtime::from_state(Arc::new(state), deps));

    MultiLayerFixture {
        runtime,
        render_log,
        layer_ids,
        page_keys: page_object_keys,
    }
}

pub fn build_multi_layer_config(layer_ids: &[LayerId], binding_ids: &[BindingId]) -> Config {
    let mut size_per_band = BTreeMap::new();
    size_per_band.insert("hi".into(), "1024m".into());
    let layers: Vec<Layer> = layer_ids
        .iter()
        .zip(binding_ids.iter())
        .enumerate()
        .map(|(i, (lid, bid))| Layer {
            name: lid.clone(),
            title: format!("Layer {i}"),
            abstract_: String::new(),
            kind: "polygon".into(),
            scale: None,
            group: None,
            bbox: None,
            sources: vec![SourceBinding {
                source: mars_config::SourceId::new("default"),
                scale: None,
                band: None,
                max_denom: None,
                filter: None,
                from: Some(bid.as_str().into()),
                sql: None,
                uri: None,
                format: None,
                source_crs: None,
                geometry_column: "geom".into(),
                id_column: None,
                attributes: vec!["name".into()],
                levels: None,
                page_size_target_bytes: None,
                reconcile_every_cycles: None,
                sidecar_size_warn_bytes: None,
                simplifier: None,
                on_missing_page: None,
            }],
            classes: vec![Class {
                name: "main".into(),
                title: String::new(),
                when: None,
                scale: None,
                style: ClassStyle::Inline(default_style()),
                label: None,
            }],
            label: None,
            label_survival: LabelSurvival::Independent,
            raster: None,
            wms: Default::default(),
        })
        .collect();
    Config {
        service: ServiceMeta {
            name: "test-multi".into(),
            ..Default::default()
        },
        sources: vec![Source {
            id: mars_config::SourceId::new("default"),
            native_crs: CrsCode::new(REQUEST_CRS),
            backend: mars_config::SourceBackend::Postgis(mars_config::PostgisBackend {
                dsn: "memory://".into(),
                change_feed: None,
                pool: Default::default(),
                bootstrap: None,
            }),
        }],
        artifacts: Artifacts {
            store: ArtifactStore {
                kind: "fs".into(),
                endpoint: None,
                bucket: None,
                prefix: None,
                path: Some("/tmp".into()),
                allow_http: false,
                ..Default::default()
            },
            cache: ArtifactCache {
                path: "/tmp".into(),
                max_size: "1GiB".into(),
                eviction: "lru".into(),
                trust_path_hash: false,
            },
        },
        scales: Scales {
            bands: vec![Band {
                name: "hi".into(),
                max_denom: 25_000,
            }],
        },
        cells: Cells {
            grid: "regular".into(),
            origin: [0.0, 0.0],
            size_per_band,
            extent: Some(Bbox::new(0.0, 0.0, 1000.0, 1000.0)),
        },
        interfaces: Interfaces::default(),
        tile_matrix_sets: Default::default(),
        reprojection: Default::default(),
        styles: Default::default(),
        layers,
        observability: Observability::default(),
        render: Render::default(),
        compiler: Compiler::default(),
    }
}

pub fn build_multi_layer_stylesheet(n_layers: usize) -> Stylesheet {
    let mut ss = Stylesheet::default();
    for i in 0..n_layers {
        // distinct fill colour per layer so tests can recover layer index
        // from the emitted DrawOp::Path style.
        let style = Style {
            fill: Some(FillPaint::Solid(Colour {
                r: (10 * (i + 1)) as u8,
                g: 0,
                b: 0,
                a: 255,
            })),
            ..Default::default()
        };
        ss.geometry.insert(format!("layer_{i}__main"), Arc::from(vec![style]));
    }
    ss
}
