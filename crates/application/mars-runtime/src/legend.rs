//! WMS GetLegendGraphic swatch composition.
//!
//! For each layer class we render a small swatch keyed on the layer's
//! geometry kind: polygon -> filled rectangle, line -> a horizontal stroke,
//! point -> a centred filled circle. Multiple classes are stacked into a
//! single canvas with the class title printed next to its swatch.
//!
//! No new ports: rendering goes through [`mars_render_port::Renderer`] and
//! encoding through [`mars_render_port::Encoder`]. v1 is uncached and
//! computed on every request; a precomputed swatch artifact is a follow-up
//! once legend traffic justifies it.

use std::sync::Arc;

use mars_config::{Class, ClassStyle, Config, Layer};
use mars_render_port::{Canvas, DrawOp, Path as RPath, Renderer, Subpath};
use mars_style::{Colour, LabelStyle, LayerGeomKind, ResolvedStyle, Stylesheet};
use mars_types::{ImageFormat, LayerId};

/// legends are scale-agnostic thumbnails. resolve at denom=0 so
/// `ScaledSize::resolve` skips ref_denom scaling and only applies clamps.
const LEGEND_DENOM: u64 = 0;

use crate::{Deps, RuntimeError};

/// Parsed WMS GetLegendGraphic request.
#[derive(Debug, Clone)]
pub struct LegendPlan {
    /// Single layer the legend describes. WMS spec is single-layer per request.
    pub layer: LayerId,
    /// Image format for the output.
    pub format: ImageFormat,
    /// Width of each class swatch in pixels.
    pub swatch_width: u32,
    /// Height of each class swatch in pixels.
    pub swatch_height: u32,
    /// Optional restriction to a single class by name (`RULE=`).
    pub rule: Option<String>,
}

impl LegendPlan {
    /// Default swatch box (matches MapServer's default ~20 px).
    pub const DEFAULT_SWATCH_WIDTH: u32 = 20;
    /// Default swatch box height.
    pub const DEFAULT_SWATCH_HEIGHT: u32 = 20;
}

/// Render the legend image. The stylesheet is passed explicitly so callers
/// can route either through the active runtime state (production) or supply
/// an empty default for tests that only use inline-styled classes.
pub fn render_legend(
    plan: &LegendPlan,
    cfg: &Config,
    stylesheet: &Stylesheet,
    deps: &Deps,
) -> Result<Vec<u8>, RuntimeError> {
    let layer = cfg
        .layers
        .iter()
        .find(|l| l.name == plan.layer)
        .ok_or_else(|| RuntimeError::LayerNotDefined {
            layer: plan.layer.as_str().to_owned(),
        })?;
    let kind = LayerGeomKind::parse(layer.kind.as_str()).unwrap_or(LayerGeomKind::Polygon);
    let entries = build_entries(layer, plan.rule.as_deref(), stylesheet)?;

    let layout = LegendLayout::compute(plan.swatch_width, plan.swatch_height, &entries, deps.renderer.as_ref())?;
    let canvas = Canvas {
        width: layout.total_width,
        height: layout.total_height,
        background: Some(Colour::rgba(0xff, 0xff, 0xff, 0xff)),
    };

    let mut ops: Vec<DrawOp> = Vec::with_capacity(entries.len() * 2);
    let label_style = Arc::new(default_label_style().resolve(LEGEND_DENOM));
    for (i, entry) in entries.iter().enumerate() {
        let row_top = (i as u32) * layout.row_height;
        push_swatch_ops(
            &mut ops,
            kind,
            entry.style.clone(),
            layout.swatch_padding,
            row_top + layout.swatch_padding,
            plan.swatch_width,
            plan.swatch_height,
        );
        if !entry.title.is_empty() {
            ops.push(DrawOp::Label {
                anchor: (layout.label_x as f32, (row_top + layout.label_baseline_offset()) as f32),
                text: entry.title.clone(),
                style: label_style.clone(),
                angle_rad: 0.0,
            });
        }
    }

    let pixmap = deps.renderer.render(canvas, &ops)?;
    Ok(deps.encoder.encode(&pixmap, plan.format)?)
}

struct ClassEntry {
    title: String,
    style: Arc<ResolvedStyle>,
}

fn build_entries(layer: &Layer, rule: Option<&str>, stylesheet: &Stylesheet) -> Result<Vec<ClassEntry>, RuntimeError> {
    let classes: Vec<&Class> = match rule {
        Some(r) => layer.classes.iter().filter(|c| c.name == r).collect(),
        None => layer.classes.iter().collect(),
    };
    // empty class list collapses to a single default swatch so the response
    // is never zero-area. clients always get one row at the configured size.
    if classes.is_empty() {
        return Ok(vec![ClassEntry {
            title: layer.title.clone(),
            style: Arc::new(mars_style::Style::default().resolve(LEGEND_DENOM)),
        }]);
    }
    classes
        .into_iter()
        .map(|c| {
            let title = if c.title.is_empty() {
                c.name.clone()
            } else {
                c.title.clone()
            };
            let style = resolve_style(c, stylesheet)?;
            Ok(ClassEntry { title, style })
        })
        .collect()
}

fn resolve_style(class: &Class, stylesheet: &Stylesheet) -> Result<Arc<ResolvedStyle>, RuntimeError> {
    // legend swatches are single-style. for multi-pass class definitions we
    // take the first pass so the legend remains a thumbnail of the dominant
    // paint rather than a stack of overlapping swatches; mirrors the
    // mapserver legend image, which emits the first STYLE only.
    match &class.style {
        ClassStyle::Inline(s) => Ok(Arc::new(s.resolve(LEGEND_DENOM))),
        ClassStyle::Passes { passes } => passes
            .first()
            .map(|s| Arc::new(s.resolve(LEGEND_DENOM)))
            .ok_or_else(|| RuntimeError::StylesheetDrift {
                layer: class.name.clone(),
                name: "<empty passes>".into(),
            }),
        ClassStyle::Ref { name } => {
            let passes = stylesheet
                .geometry
                .get(name)
                .ok_or_else(|| RuntimeError::StylesheetDrift {
                    layer: class.name.clone(),
                    name: name.clone(),
                })?;
            passes
                .first()
                .map(|s| Arc::new(s.resolve(LEGEND_DENOM)))
                .ok_or_else(|| RuntimeError::StylesheetDrift {
                    layer: class.name.clone(),
                    name: name.clone(),
                })
        }
    }
}

struct LegendLayout {
    total_width: u32,
    total_height: u32,
    row_height: u32,
    swatch_padding: u32,
    label_x: u32,
}

impl LegendLayout {
    fn compute(
        swatch_w: u32,
        swatch_h: u32,
        entries: &[ClassEntry],
        renderer: &dyn Renderer,
    ) -> Result<Self, RuntimeError> {
        let label_style = default_label_style().resolve(LEGEND_DENOM);
        let mut max_label_w: f32 = 0.0;
        for e in entries {
            if e.title.is_empty() {
                continue;
            }
            let metrics = renderer.measure_text(&e.title, &label_style)?;
            if metrics.advance_x > max_label_w {
                max_label_w = metrics.advance_x;
            }
        }
        let swatch_padding = 4u32;
        let label_padding = 8u32;
        let row_height = swatch_h + 2 * swatch_padding;
        let label_w = max_label_w.ceil() as u32;
        let label_x = swatch_padding + swatch_w + label_padding;
        let total_width = (label_x + label_w + swatch_padding).max(swatch_w + 2 * swatch_padding);
        let total_height = row_height.saturating_mul(entries.len().max(1) as u32);
        Ok(Self {
            total_width,
            total_height,
            row_height,
            swatch_padding,
            label_x,
        })
    }

    fn label_baseline_offset(&self) -> u32 {
        // baseline ~70% down the row; lines up with default-font ascent.
        (self.row_height as f32 * 0.7) as u32
    }
}

fn push_swatch_ops(
    ops: &mut Vec<DrawOp>,
    kind: LayerGeomKind,
    style: Arc<ResolvedStyle>,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) {
    let xf = x as f32;
    let yf = y as f32;
    let wf = w as f32;
    let hf = h as f32;
    match kind {
        LayerGeomKind::Polygon => {
            let path = RPath {
                subpaths: vec![Subpath {
                    points: vec![(xf, yf), (xf + wf, yf), (xf + wf, yf + hf), (xf, yf + hf), (xf, yf)],
                    closed: true,
                }],
            };
            ops.push(DrawOp::Path { path, style });
        }
        LayerGeomKind::Line => {
            // open subpath so the renderer strokes only.
            let mid = yf + hf * 0.5;
            let path = RPath {
                subpaths: vec![Subpath {
                    points: vec![(xf, mid), (xf + wf, mid)],
                    closed: false,
                }],
            };
            ops.push(DrawOp::Path { path, style });
        }
        LayerGeomKind::Point => {
            // 16-vertex circle approximation; legibility-grade, not sub-pixel.
            let cx = xf + wf * 0.5;
            let cy = yf + hf * 0.5;
            let r = wf.min(hf) * 0.4;
            let n = 16usize;
            let mut points = Vec::with_capacity(n + 1);
            for i in 0..=n {
                let theta = std::f32::consts::TAU * (i as f32) / (n as f32);
                points.push((cx + r * theta.cos(), cy + r * theta.sin()));
            }
            let path = RPath {
                subpaths: vec![Subpath { points, closed: true }],
            };
            ops.push(DrawOp::Path { path, style });
        }
    }
}

fn default_label_style() -> LabelStyle {
    LabelStyle {
        font_family: "sans".to_owned(),
        font_size: 12.0.into(),
        fill: Colour::rgb(0x20, 0x20, 0x20),
        halo: None,
        priority: 0,
        min_distance: 0.0,
        position: mars_style::AnchorPosition::default(),
        offset_px: (0.0, 0.0),
        angle: None,
        partials: false,
        force: false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
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
cells:
  grid: regular
  origin: [0, 0]
  size_per_band: { hi: 1024m }
interfaces: {}
reprojection:
  allowlist: [EPSG:25832]
layers:
  - name: roads
    title: "Roads"
    type: polygon
    sources: [{ from: t, geometry_column: g }]
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
}
