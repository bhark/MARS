//! per-request render orchestration.
//!
//! pulls the threads of `state` (binding/level/page index), `plan` (level
//! pick and viewport intersection), and `fetch` (page bytes via the cache
//! port) together into the actual render pipeline that `Runtime::render`
//! exposes. style join and labels arrive in D5 / D6; this module's render
//! path stops at "raw geometry, fallback style" so the spine is verifiable
//! in isolation.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use mars_artifact::{
    ArtifactReader, GeomKind, SectionKind, SpatialIndex, decode_class_assignment, decode_geometry_at_slots,
    decode_style_refs,
};
use mars_config::Layer;
use mars_render_port::{Canvas, DrawOp, Path, Subpath};
use mars_style::{Colour, Style, Stylesheet};
use mars_types::{Bbox, BindingMetadata, LayerId, PageEntry};

use crate::state::RuntimeState;
use crate::{Deps, RenderPlan, RuntimeError};
use crate::{fetch::fetch_page, fetch::fetch_sidecar, plan as planning};

/// fallback style used until D5 wires the class-sidecar join. blue fill +
/// dark stroke so the spine is clearly visible against the white default
/// background of the test fixtures.
fn fallback_style() -> Arc<Style> {
    Arc::new(Style {
        fill: Some(Colour {
            r: 64,
            g: 128,
            b: 220,
            a: 200,
        }),
        stroke: Some(Colour {
            r: 32,
            g: 64,
            b: 110,
            a: 255,
        }),
        stroke_width: Some(1.0),
        stroke_dasharray: None,
        stroke_linecap: None,
        stroke_linejoin: None,
    })
}

/// drive one render plan end-to-end. produces encoded image bytes ready to
/// hand back to the WMS / WMTS interface.
pub(crate) async fn render_plan(
    state: &RuntimeState,
    deps: &Deps,
    plan: &RenderPlan,
) -> Result<Vec<u8>, RuntimeError> {
    let config = state.config_or_err()?;
    let canvas = Canvas {
        width: plan.width,
        height: plan.height,
        background: None,
    };
    let mut all_ops: Vec<DrawOp> = Vec::new();
    let fallback = fallback_style();
    for layer_id in &plan.layers {
        let layer_cfg = lookup_layer(config, layer_id)?;
        let denom = crate::denom_from_plan(plan.bbox.width(), plan.width);
        let Some((binding_id, level)) = planning::pick_binding_and_level(layer_cfg, denom, state) else {
            // no binding covers this layer at this scale; render nothing.
            continue;
        };
        let binding = state
            .index
            .binding(&state.manifest, &binding_id)
            .ok_or_else(|| RuntimeError::InvalidManifest {
                reason: format!(
                    "selected binding `{binding_id}` for layer `{layer}` is not in manifest",
                    layer = layer_id
                ),
            })?;
        let native_viewport = planning::reproject_viewport(plan.bbox, &plan.crs, &binding.native_crs)?;
        let pages = planning::resolve_pages(state, &binding_id, level, native_viewport);
        if pages.is_empty() {
            continue;
        }
        let layer_ops = render_layer_pages(deps, state, layer_id, binding, &pages, plan, &fallback).await?;
        all_ops.extend(layer_ops);
    }

    let pixmap = deps.renderer.render(canvas, &all_ops)?;
    let bytes = deps.encoder.encode(&pixmap, plan.format)?;
    Ok(bytes)
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

async fn render_layer_pages(
    deps: &Deps,
    state: &RuntimeState,
    layer_id: &LayerId,
    binding: &BindingMetadata,
    pages: &[&PageEntry],
    plan: &RenderPlan,
    fallback: &Arc<Style>,
) -> Result<Vec<DrawOp>, RuntimeError> {
    let mut futs = FuturesUnordered::new();
    for page in pages {
        let store = deps.store.clone();
        let cache = deps.cache.clone();
        let entry = (*page).clone();
        let class_entry = state
            .index
            .class_sidecar(&state.manifest, layer_id, &entry.key)
            .cloned();
        futs.push(async move {
            let page_bytes = fetch_page(&cache, &store, &entry).await?;
            let class_bytes = match &class_entry {
                Some(e) => Some(fetch_sidecar(&cache, &store, e).await?),
                None => None,
            };
            Ok::<_, RuntimeError>((entry, page_bytes, class_bytes))
        });
    }
    let mut all_ops: Vec<DrawOp> = Vec::new();
    let same_crs = binding.native_crs.as_str() == plan.crs.as_str();
    while let Some(res) = futs.next().await {
        let (entry, page_bytes, class_bytes) = res?;
        let class = match class_bytes {
            Some(b) => Some(ClassResolver::open(b)?),
            None => None,
        };
        let mut page_ops = decode_page_to_ops(
            page_bytes,
            &entry,
            plan,
            binding,
            class.as_ref(),
            &state.stylesheet,
            fallback,
            same_crs,
        )?;
        all_ops.append(&mut page_ops);
    }
    Ok(all_ops)
}

/// resolves `feature_id -> Style` by binary-searching the class assignment
/// table and looking the resulting style ref up in the active stylesheet.
struct ClassResolver {
    /// `(feature_id, class_index)` pairs sorted ascending by feature_id.
    assignments: Vec<(u64, u16)>,
    /// `class_index` indexes into this list to get a stylesheet ref name.
    style_refs: Vec<String>,
}

impl ClassResolver {
    fn open(bytes: Bytes) -> Result<Self, RuntimeError> {
        let reader = ArtifactReader::open(bytes).map_err(map_artifact_err)?;
        let class_bytes = reader
            .section(SectionKind::ClassAssignment)
            .map_err(map_artifact_err)?;
        let style_refs_bytes = reader.section(SectionKind::StyleRefs).map_err(map_artifact_err)?;
        let assignments = decode_class_assignment(&class_bytes).map_err(map_artifact_err)?;
        let style_refs = decode_style_refs(&style_refs_bytes).map_err(map_artifact_err)?;
        Ok(Self {
            assignments,
            style_refs,
        })
    }

    fn style_ref_for(&self, feature_id: u64) -> Option<&str> {
        let pos = self
            .assignments
            .binary_search_by_key(&feature_id, |&(id, _)| id)
            .ok()?;
        let cls = self.assignments[pos].1 as usize;
        self.style_refs.get(cls).map(String::as_str)
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_page_to_ops(
    bytes: Bytes,
    page: &PageEntry,
    plan: &RenderPlan,
    binding: &BindingMetadata,
    class: Option<&ClassResolver>,
    stylesheet: &Stylesheet,
    fallback: &Arc<Style>,
    same_crs: bool,
) -> Result<Vec<DrawOp>, RuntimeError> {
    let reader = ArtifactReader::open(bytes).map_err(map_artifact_err)?;
    let spatial_bytes = reader.section(SectionKind::SpatialIndex).map_err(map_artifact_err)?;
    let geom_bytes = reader.section(SectionKind::GeometryPayload).map_err(map_artifact_err)?;
    let idx = SpatialIndex::open(spatial_bytes).map_err(map_artifact_err)?;
    if idx.is_empty() {
        return Ok(Vec::new());
    }
    let qbb = bbox_native(plan.bbox, &plan.crs, &binding.native_crs)?;
    let qbb_f32 = bbox_to_f32(qbb);
    let mut slots: Vec<u32> = Vec::new();
    idx.query(qbb_f32, &mut slots);
    if slots.is_empty() {
        // page bbox claimed intersection but the R-tree disagrees; bail out.
        let _ = page;
        return Ok(Vec::new());
    }
    let features = decode_geometry_at_slots(&geom_bytes, &slots).map_err(map_artifact_err)?;
    let projected = if same_crs {
        features
    } else {
        project_features(features, &binding.native_crs, &plan.crs)?
    };
    let mut ops = Vec::with_capacity(projected.len());
    for f in projected {
        let style = class
            .and_then(|c| c.style_ref_for(f.id))
            .and_then(|name| stylesheet.geometry.get(name).cloned())
            .unwrap_or_else(|| fallback.clone());
        if let Some(op) = feature_to_drawop(&f.geom, plan.bbox, plan.width, plan.height, style) {
            ops.push(op);
        }
    }
    Ok(ops)
}

fn bbox_native(viewport: Bbox, from: &mars_types::CrsCode, to: &mars_types::CrsCode) -> Result<Bbox, RuntimeError> {
    planning::reproject_viewport(viewport, from, to)
}

fn bbox_to_f32(b: Bbox) -> [f32; 4] {
    [b.min_x as f32, b.min_y as f32, b.max_x as f32, b.max_y as f32]
}

fn project_features(
    features: Vec<mars_artifact::FeatureGeom>,
    from: &mars_types::CrsCode,
    to: &mars_types::CrsCode,
) -> Result<Vec<mars_artifact::FeatureGeom>, RuntimeError> {
    let xform = mars_proj::cached_transformer(from, to).map_err(map_proj_err)?;
    let mut out = Vec::with_capacity(features.len());
    for f in features {
        let geom = project_geom(&f.geom, &xform)?;
        out.push(mars_artifact::FeatureGeom {
            id: f.id,
            bbox: f.bbox,
            geom,
        });
    }
    Ok(out)
}

fn project_geom(g: &GeomKind, xform: &mars_proj::Transformer) -> Result<GeomKind, RuntimeError> {
    let mapped = match g {
        GeomKind::Point(c) => GeomKind::Point(project_point(*c, xform)?),
        GeomKind::LineString(coords) => GeomKind::LineString(project_ring(coords, xform)?),
        GeomKind::Polygon(rings) => {
            let mut out = Vec::with_capacity(rings.len());
            for ring in rings {
                out.push(project_ring(ring, xform)?);
            }
            GeomKind::Polygon(out)
        }
        GeomKind::MultiPoint(coords) => GeomKind::MultiPoint(project_ring(coords, xform)?),
        GeomKind::MultiLineString(parts) => {
            let mut out = Vec::with_capacity(parts.len());
            for ring in parts {
                out.push(project_ring(ring, xform)?);
            }
            GeomKind::MultiLineString(out)
        }
        GeomKind::MultiPolygon(parts) => {
            let mut out = Vec::with_capacity(parts.len());
            for poly in parts {
                let mut rings = Vec::with_capacity(poly.len());
                for ring in poly {
                    rings.push(project_ring(ring, xform)?);
                }
                out.push(rings);
            }
            GeomKind::MultiPolygon(out)
        }
    };
    Ok(mapped)
}

fn project_point(c: (f64, f64), xform: &mars_proj::Transformer) -> Result<(f64, f64), RuntimeError> {
    xform.transform_point(c.0, c.1).map_err(map_proj_err)
}

fn project_ring(coords: &[(f64, f64)], xform: &mars_proj::Transformer) -> Result<Vec<(f64, f64)>, RuntimeError> {
    if coords.is_empty() {
        return Ok(Vec::new());
    }
    let mut buf: Vec<[f64; 2]> = coords.iter().map(|&(x, y)| [x, y]).collect();
    xform.transform_points(&mut buf).map_err(map_proj_err)?;
    Ok(buf.into_iter().map(|p| (p[0], p[1])).collect())
}

fn feature_to_drawop(g: &GeomKind, viewport: Bbox, w: u32, h: u32, style: Arc<Style>) -> Option<DrawOp> {
    let path = match g {
        GeomKind::Point(c) => single_point_path(*c, viewport, w, h),
        GeomKind::LineString(coords) => Path {
            subpaths: vec![ring_to_subpath(coords, viewport, w, h, false)],
        },
        GeomKind::Polygon(rings) => Path {
            subpaths: rings
                .iter()
                .map(|r| ring_to_subpath(r, viewport, w, h, true))
                .collect(),
        },
        GeomKind::MultiPoint(coords) => Path {
            subpaths: coords
                .iter()
                .map(|&c| Subpath {
                    points: vec![world_to_pixel(c, viewport, w, h)],
                    closed: false,
                })
                .collect(),
        },
        GeomKind::MultiLineString(parts) => Path {
            subpaths: parts
                .iter()
                .map(|r| ring_to_subpath(r, viewport, w, h, false))
                .collect(),
        },
        GeomKind::MultiPolygon(parts) => Path {
            subpaths: parts
                .iter()
                .flat_map(|poly| poly.iter().map(|r| ring_to_subpath(r, viewport, w, h, true)))
                .collect(),
        },
    };
    if path.subpaths.is_empty() {
        return None;
    }
    Some(DrawOp::Path { path, style })
}

fn ring_to_subpath(coords: &[(f64, f64)], viewport: Bbox, w: u32, h: u32, closed: bool) -> Subpath {
    Subpath {
        points: coords.iter().map(|&c| world_to_pixel(c, viewport, w, h)).collect(),
        closed,
    }
}

fn single_point_path(c: (f64, f64), viewport: Bbox, w: u32, h: u32) -> Path {
    Path {
        subpaths: vec![Subpath {
            points: vec![world_to_pixel(c, viewport, w, h)],
            closed: false,
        }],
    }
}

fn world_to_pixel(c: (f64, f64), viewport: Bbox, w: u32, h: u32) -> (f32, f32) {
    let dx = viewport.width();
    let dy = viewport.height();
    if !dx.is_finite() || !dy.is_finite() || dx <= 0.0 || dy <= 0.0 {
        return (0.0, 0.0);
    }
    let nx = (c.0 - viewport.min_x) / dx;
    let ny = (c.1 - viewport.min_y) / dy;
    let px = nx * f64::from(w);
    let py = (1.0 - ny) * f64::from(h);
    (px as f32, py as f32)
}

fn map_artifact_err(e: mars_artifact::ArtifactError) -> RuntimeError {
    RuntimeError::InvalidManifest {
        reason: format!("artifact decode error: {e}"),
    }
}

fn map_proj_err(e: mars_proj::ProjError) -> RuntimeError {
    RuntimeError::InvalidManifest {
        reason: format!("projection error: {e}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn world_to_pixel_origin_top_left() {
        let v = Bbox::new(0.0, 0.0, 10.0, 10.0);
        let (px, py) = world_to_pixel((0.0, 10.0), v, 100, 100);
        assert!(px.abs() < 0.001);
        assert!(py.abs() < 0.001);
    }

    #[test]
    fn world_to_pixel_far_corner_bottom_right() {
        let v = Bbox::new(0.0, 0.0, 10.0, 10.0);
        let (px, py) = world_to_pixel((10.0, 0.0), v, 100, 100);
        assert!((px - 100.0).abs() < 0.001);
        assert!((py - 100.0).abs() < 0.001);
    }

    #[test]
    fn world_to_pixel_clamps_degenerate_viewport() {
        let v = Bbox::new(0.0, 0.0, 0.0, 0.0);
        assert_eq!(world_to_pixel((1.0, 1.0), v, 10, 10), (0.0, 0.0));
    }

    #[test]
    fn feature_to_drawop_polygon_rings_closed() {
        let geom = GeomKind::Polygon(vec![vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)]]);
        let v = Bbox::new(0.0, 0.0, 10.0, 10.0);
        let op = feature_to_drawop(&geom, v, 100, 100, fallback_style()).unwrap();
        match op {
            DrawOp::Path { path, .. } => {
                assert_eq!(path.subpaths.len(), 1);
                assert!(path.subpaths[0].closed);
                assert_eq!(path.subpaths[0].points.len(), 5);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn feature_to_drawop_linestring_open() {
        let geom = GeomKind::LineString(vec![(0.0, 0.0), (10.0, 10.0)]);
        let v = Bbox::new(0.0, 0.0, 10.0, 10.0);
        let op = feature_to_drawop(&geom, v, 100, 100, fallback_style()).unwrap();
        if let DrawOp::Path { path, .. } = op {
            assert_eq!(path.subpaths.len(), 1);
            assert!(!path.subpaths[0].closed);
        } else {
            panic!("expected DrawOp::Path");
        }
    }
}
