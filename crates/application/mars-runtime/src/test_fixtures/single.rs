//! single-layer in-memory fixture: one layer, one binding, one page with
//! `N` polygon features and a label per feature. backs `build_fixture` and
//! `build_fixture_with`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::Mutex;
use std::time::SystemTime;

use mars_artifact::{
    ArtifactKind, ArtifactWriter, AttrValue, FeatureGeom, GeomKind, LabelCandidate, LabelShape, SpatialIndexBuilder,
    encode_row,
};
use mars_config::model::Config;
use mars_render_port::{DrawOp, Encoder, Renderer};
use mars_store::mem::{InMemoryCache, InMemoryStore};
use mars_store::{LocalCache, ObjectStore};
use mars_style::LabelSurvival;
use mars_test_support::port_fakes::{CapturingRenderer, StubEncoder};
use mars_types::{
    Bbox, BindingId, BindingMetadata, CrsCode, DecimationLevel, HilbertKey, ImageFormat as TImageFormat, LayerId,
    LayerSidecarEntry, LayerSidecarKind, MANIFEST_FORMAT_VERSION, Manifest, PageEntry, PageId, PageKey,
};

use crate::{Deps, Fonts, RenderPlan, Runtime, RuntimeState};

use super::REQUEST_CRS;
use super::config::{build_minimal_config, build_minimal_stylesheet};

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

/// build the default fixture: one layer, one binding, one page with three
/// polygon features and a label per feature.
pub async fn build_fixture() -> Fixture {
    build_fixture_with(FixtureOptions::default()).await
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
        raster_sources: crate::RasterSourceRegistry::new(),
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
