//! multi-layer in-memory fixture: N independent layers, one binding/page/
//! feature/stylesheet entry each. used to exercise per-layer parallelism
//! in the runtime render path (each layer fetched, decoded, and emitted
//! independently).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::Mutex;
use std::time::SystemTime;

use mars_artifact::{ArtifactKind, ArtifactWriter, AttrValue, FeatureGeom, GeomKind, SpatialIndexBuilder, encode_row};
use mars_config::model::{Config, Layer};
use mars_render_port::{DrawOp, Encoder, Renderer};
use mars_store::mem::{InMemoryCache, InMemoryStore};
use mars_store::{LocalCache, ObjectStore};
use mars_style::{Colour, FillPaint, LabelSurvival, Style, Stylesheet};
use mars_test_support::port_fakes::{CapturingRenderer, StubEncoder};
use mars_types::{
    ArtifactKey, Bbox, BindingId, BindingMetadata, CrsCode, DecimationLevel, HilbertKey, ImageFormat as TImageFormat,
    LayerId, LayerSidecarEntry, LayerSidecarKind, MANIFEST_FORMAT_VERSION, Manifest, PageEntry, PageId, PageKey,
};

use crate::{Deps, Fonts, RenderPlan, Runtime, RuntimeState};

use super::REQUEST_CRS;
use super::config::{base_config, default_main_class, default_source_binding};

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
        raster_sources: crate::RasterSourceRegistry::new(),
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
            sources: vec![default_source_binding(bid)],
            classes: vec![default_main_class()],
            label: None,
            label_survival: LabelSurvival::Independent,
            raster: None,
            wms: Default::default(),
            ows: Default::default(),
            template: None,
        })
        .collect();
    let mut config = base_config("test-multi");
    config.layers = layers;
    config
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
