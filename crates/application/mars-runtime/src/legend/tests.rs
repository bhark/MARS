#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;
use mars_observability::Metrics;
use mars_render_port::{EncodeError, Encoder, Pixmap, RenderError, TextMetrics};
use mars_style::ResolvedLabelStyle;
use mars_test_support::port_fakes::{NotImplementedCache, NotImplementedStore};
use mars_text::Fonts;

#[derive(Debug, Default)]
struct CountingRenderer {
    ops_observed: std::sync::atomic::AtomicUsize,
}

impl Renderer for CountingRenderer {
    fn render(&self, canvas: Canvas, ops: &[DrawOp]) -> Result<Pixmap, RenderError> {
        self.ops_observed.store(ops.len(), std::sync::atomic::Ordering::Relaxed);
        Ok(Pixmap {
            width: canvas.width,
            height: canvas.height,
            premultiplied_rgba: vec![0; (canvas.width as usize) * (canvas.height as usize) * 4],
        })
    }
    fn measure_text(&self, text: &str, style: &ResolvedLabelStyle) -> Result<TextMetrics, RenderError> {
        Ok(TextMetrics {
            advance_x: (text.chars().count() as f32) * style.font_size * 0.55,
            ascent: style.font_size * 0.8,
            descent: style.font_size * 0.2,
        })
    }
}

#[derive(Debug)]
struct PassthroughEncoder;

impl Encoder for PassthroughEncoder {
    fn encode(&self, pixmap: &Pixmap, _format: ImageFormat) -> Result<Vec<u8>, EncodeError> {
        let mut v = Vec::with_capacity(8);
        v.extend_from_slice(&pixmap.width.to_le_bytes());
        v.extend_from_slice(&pixmap.height.to_le_bytes());
        Ok(v)
    }
}

fn deps_with(renderer: Arc<CountingRenderer>) -> Deps {
    let metrics = Metrics::new().unwrap();
    Deps {
        store: Arc::new(NotImplementedStore),
        cache: Arc::new(NotImplementedCache),
        renderer,
        encoder: Arc::new(PassthroughEncoder),
        metrics,
        fonts: Arc::new(Fonts::with_default()),
        images: Arc::new(crate::images::MutableImageRegistry::new()),
        raster_sources: crate::RasterSourceRegistry::new(),
    }
}

fn polygon_layer_cfg() -> Config {
    let yaml = r##"
service: { name: t, title: T, abstract: A, contact_email: "" }
sources:
  - { id: default, type: postgis, dsn: "postgres://x", native_crs: EPSG:25832 }
artifacts:
  store: { type: fs, path: /tmp }
  cache: { path: /tmp/c, max_size: 1GiB }
scales:
  bands: [{ name: hi, max_denom_exclusive: 25000 }]
interfaces: {}
reprojection:
  allowlist: [EPSG:25832]
layers:
  - name: roads
    title: "Roads"
    type: polygon
    sources: [{ kind: postgis_table, from: t, geometry_column: g }]
    classes:
      - { name: main, title: "Main", style: { type: inline, fill: "#aabbcc", stroke: "#000000" } }
      - { name: minor, title: "Minor", style: { type: inline, stroke: "#555555" } }
"##;
    serde_yaml_ng::from_str(yaml).unwrap()
}

#[test]
fn renders_one_swatch_plus_label_per_class() {
    let cfg = polygon_layer_cfg();
    let renderer = Arc::new(CountingRenderer::default());
    let deps = deps_with(renderer.clone());
    let plan = LegendPlan {
        layer: LayerId::new("roads"),
        format: ImageFormat::Png,
        swatch_width: LegendPlan::DEFAULT_SWATCH_WIDTH,
        swatch_height: LegendPlan::DEFAULT_SWATCH_HEIGHT,
        rule: None,
    };
    let bytes = render_legend(&plan, &cfg, &Stylesheet::default(), &deps).unwrap();
    assert_eq!(renderer.ops_observed.load(std::sync::atomic::Ordering::Relaxed), 4);
    let w = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    let h = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    assert!(w >= LegendPlan::DEFAULT_SWATCH_WIDTH);
    assert!(h >= 2 * (LegendPlan::DEFAULT_SWATCH_HEIGHT + 8));
}

#[test]
fn rule_filters_to_one_class() {
    let cfg = polygon_layer_cfg();
    let renderer = Arc::new(CountingRenderer::default());
    let deps = deps_with(renderer.clone());
    let plan = LegendPlan {
        layer: LayerId::new("roads"),
        format: ImageFormat::Png,
        swatch_width: LegendPlan::DEFAULT_SWATCH_WIDTH,
        swatch_height: LegendPlan::DEFAULT_SWATCH_HEIGHT,
        rule: Some("main".into()),
    };
    let _ = render_legend(&plan, &cfg, &Stylesheet::default(), &deps).unwrap();
    assert_eq!(renderer.ops_observed.load(std::sync::atomic::Ordering::Relaxed), 2);
}

#[test]
fn unknown_layer_returns_layer_not_defined() {
    let cfg = polygon_layer_cfg();
    let deps = deps_with(Arc::new(CountingRenderer::default()));
    let plan = LegendPlan {
        layer: LayerId::new("ghost"),
        format: ImageFormat::Png,
        swatch_width: 20,
        swatch_height: 20,
        rule: None,
    };
    let err = render_legend(&plan, &cfg, &Stylesheet::default(), &deps).unwrap_err();
    assert!(matches!(err, RuntimeError::LayerNotDefined { .. }));
}

#[test]
fn ref_to_missing_stylesheet_entry_surfaces_drift() {
    let mut cfg = polygon_layer_cfg();
    cfg.layers[0].classes.push(Class {
        name: "named".into(),
        title: String::new(),
        when: None,
        scale: None,
        style: ClassStyle::Ref { name: "absent".into() },
        label: None,
    });
    let deps = deps_with(Arc::new(CountingRenderer::default()));
    let plan = LegendPlan {
        layer: LayerId::new("roads"),
        format: ImageFormat::Png,
        swatch_width: 20,
        swatch_height: 20,
        rule: Some("named".into()),
    };
    let err = render_legend(&plan, &cfg, &Stylesheet::default(), &deps).unwrap_err();
    assert!(matches!(err, RuntimeError::StylesheetDrift { .. }));
}
