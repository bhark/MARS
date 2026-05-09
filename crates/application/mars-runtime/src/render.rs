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
use mars_artifact::{
    ArtifactReader, FeatureGeom, GeomKind, LabelCandidate, LabelShape, SectionKind, SpatialIndex,
    decode_class_assignment, decode_label_candidates, decode_one_geom, decode_style_refs, iter_feature_index,
};
use mars_config::Layer;
use mars_render_port::{Canvas, DrawOp, Path, Renderer, Subpath};
use mars_style::{Colour, LabelStyle, LabelSurvival, Style, Stylesheet};
use mars_types::{Bbox, BindingMetadata, LayerId, PageEntry};

use crate::state::RuntimeState;
use crate::{Deps, RenderPlan, RuntimeError};
use crate::{fetch::fetch_page, fetch::fetch_sidecar, plan as planning};

/// fallback style used when no class sidecar binds the feature to a named
/// stylesheet entry. blue fill + dark stroke so the spine stays visible
/// against the white default background of the test fixtures.
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
pub(crate) async fn render_plan(state: &RuntimeState, deps: &Deps, plan: &RenderPlan) -> Result<Vec<u8>, RuntimeError> {
    let config = state.config_or_err()?;
    let page_fetch_concurrency = config.render.page_fetch_concurrency.max(1);
    let canvas = Canvas {
        width: plan.width,
        height: plan.height,
        background: None,
    };
    let mut all_ops: Vec<DrawOp> = Vec::new();
    let mut all_labels: Vec<PreparedLabel> = Vec::new();
    let fallback = fallback_style();
    for layer_id in &plan.layers {
        let layer_cfg = lookup_layer(config, layer_id)?;
        let denom = crate::denom_from_plan(plan.bbox.width(), plan.width);
        let Some((binding_id, level)) = planning::pick_binding_and_level(layer_cfg, denom, state) else {
            // no binding covers this layer at this scale; render nothing.
            continue;
        };
        let binding =
            state
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
        let layer_out = render_layer_pages(
            deps,
            state,
            layer_id,
            binding,
            &pages,
            plan,
            &fallback,
            layer_cfg.label_survival,
            deps.renderer.as_ref(),
            page_fetch_concurrency,
        )
        .await?;
        all_ops.extend(layer_out.ops);
        all_labels.extend(layer_out.labels);
    }

    // greedy collision over the accumulated label set: sort by priority
    // descending, place survivors that don't collide with already-placed
    // labels' approximate text bbox.
    let label_ops = collide_and_emit_labels(all_labels, plan.width, plan.height);
    all_ops.extend(label_ops);

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

struct LayerOutput {
    ops: Vec<DrawOp>,
    labels: Vec<PreparedLabel>,
}

#[allow(clippy::too_many_arguments)]
async fn render_layer_pages(
    deps: &Deps,
    state: &RuntimeState,
    layer_id: &LayerId,
    binding: &BindingMetadata,
    pages: &[&PageEntry],
    plan: &RenderPlan,
    fallback: &Arc<Style>,
    label_survival: LabelSurvival,
    renderer: &dyn Renderer,
    page_fetch_concurrency: usize,
) -> Result<LayerOutput, RuntimeError> {
    // ordered-and-bounded fan-out: fetch up to `page_fetch_concurrency`
    // pages in parallel but emit in input (page-key) order so draw-op
    // sequencing and equal-priority label collisions stay deterministic.
    // materialise per-page contexts up-front so the futures own all their
    // captures and don't borrow into the input slice.
    let contexts: Vec<_> = pages
        .iter()
        .map(|page| {
            let entry = (*page).clone();
            let class_entry = state
                .index
                .class_sidecar(&state.manifest, layer_id, &entry.key)
                .cloned();
            let label_entry = state
                .index
                .label_sidecar(&state.manifest, layer_id, &entry.key)
                .cloned();
            (entry, class_entry, label_entry)
        })
        .collect();
    let store = deps.store.clone();
    let cache = deps.cache.clone();
    let fetches = contexts.into_iter().map(move |(entry, class_entry, label_entry)| {
        let store = store.clone();
        let cache = cache.clone();
        async move {
            let page_bytes = fetch_page(&cache, &store, &entry).await?;
            let class_bytes = match &class_entry {
                Some(e) => Some(fetch_sidecar(&cache, &store, e).await?),
                None => None,
            };
            let label_bytes = match &label_entry {
                Some(e) => Some(fetch_sidecar(&cache, &store, e).await?),
                None => None,
            };
            Ok::<_, RuntimeError>((entry, page_bytes, class_bytes, label_bytes))
        }
    });
    let mut stream = futures_util::stream::iter(fetches).buffered(page_fetch_concurrency);

    let mut out = LayerOutput {
        ops: Vec::new(),
        labels: Vec::new(),
    };
    let same_crs = binding.native_crs.as_str() == plan.crs.as_str();
    while let Some(res) = stream.next().await {
        let (entry, page_bytes, class_bytes, label_bytes) = res?;
        let DecodedPage {
            ops: mut page_ops,
            rendered_slots,
            class,
        } = decode_page_to_ops(
            page_bytes,
            class_bytes,
            &entry,
            plan,
            binding,
            &state.stylesheet,
            fallback,
            same_crs,
        )?;
        out.ops.append(&mut page_ops);
        if let Some(bytes) = label_bytes {
            let survival_filter = match label_survival {
                LabelSurvival::Independent => None,
                LabelSurvival::FollowGeometry => Some(rendered_slots.as_slice()),
            };
            let mut prepared = prepare_labels(
                bytes,
                plan,
                binding,
                class.as_ref(),
                &state.stylesheet,
                same_crs,
                survival_filter,
                renderer,
            )?;
            out.labels.append(&mut prepared);
        }
    }
    Ok(out)
}

/// per-page render output. `rendered_slots[i]` is true when slot `i`'s
/// geometry survived the spatial-index hit-test for this page; the runtime
/// uses it as the FollowGeometry survival filter, defending the label path
/// against compiler drift between geometry and label sidecar. `class` is
/// hoisted alongside ops so the label pass can resolve style refs without
/// reopening the artifact.
struct DecodedPage {
    ops: Vec<DrawOp>,
    rendered_slots: Vec<bool>,
    class: Option<ClassResolver>,
}

/// resolves `feature_idx -> Style` by direct slot indexing on a dense
/// `Vec<Option<u16>>`, then looking the class index up in the page-local
/// style_refs table to get a stylesheet entry name.
pub(crate) struct ClassResolver {
    /// indexed by per-page slot; `None` when the slot has no class.
    by_slot: Vec<Option<u16>>,
    /// `class_index` indexes into this list to get a stylesheet ref name.
    style_refs: Vec<String>,
}

impl ClassResolver {
    fn open(bytes: Bytes, page_feature_count: usize) -> Result<Self, RuntimeError> {
        let reader = ArtifactReader::open(bytes).map_err(map_artifact_err)?;
        let class_bytes = reader.section(SectionKind::ClassAssignment).map_err(map_artifact_err)?;
        let style_refs_bytes = reader.section(SectionKind::StyleRefs).map_err(map_artifact_err)?;
        let assignments = decode_class_assignment(&class_bytes).map_err(map_artifact_err)?;
        let style_refs = decode_style_refs(&style_refs_bytes).map_err(map_artifact_err)?;
        let mut by_slot: Vec<Option<u16>> = vec![None; page_feature_count];
        for (slot, cls) in assignments {
            let s = slot as usize;
            if s < by_slot.len() {
                by_slot[s] = Some(cls);
            }
        }
        Ok(Self { by_slot, style_refs })
    }

    fn style_ref_for(&self, feature_idx: u32) -> Option<&str> {
        let cls = (*self.by_slot.get(feature_idx as usize)?)? as usize;
        self.style_refs.get(cls).map(String::as_str)
    }

    pub(crate) fn style_refs(&self) -> &[String] {
        &self.style_refs
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_page_to_ops(
    bytes: Bytes,
    class_bytes: Option<Bytes>,
    page: &PageEntry,
    plan: &RenderPlan,
    binding: &BindingMetadata,
    stylesheet: &Stylesheet,
    fallback: &Arc<Style>,
    same_crs: bool,
) -> Result<DecodedPage, RuntimeError> {
    let reader = ArtifactReader::open(bytes).map_err(map_artifact_err)?;
    let spatial_bytes = reader.section(SectionKind::SpatialIndex).map_err(map_artifact_err)?;
    let geom_bytes = reader.section(SectionKind::GeometryPayload).map_err(map_artifact_err)?;
    let idx = SpatialIndex::open(spatial_bytes).map_err(map_artifact_err)?;
    let page_feature_count = idx.len() as usize;
    let class = match class_bytes {
        Some(b) => Some(ClassResolver::open(b, page_feature_count)?),
        None => None,
    };
    if idx.is_empty() {
        return Ok(DecodedPage {
            ops: Vec::new(),
            rendered_slots: Vec::new(),
            class,
        });
    }
    let qbb = bbox_native(plan.bbox, &plan.crs, &binding.native_crs)?;
    let qbb_f32 = bbox_to_f32(qbb);
    let mut slots: Vec<u32> = Vec::new();
    idx.query(qbb_f32, &mut slots);
    if slots.is_empty() {
        // page bbox claimed intersection but the R-tree disagrees; bail out.
        let _ = page;
        return Ok(DecodedPage {
            ops: Vec::new(),
            rendered_slots: vec![false; page_feature_count],
            class,
        });
    }
    slots.sort_unstable();
    slots.dedup();
    let mut rendered_slots = vec![false; page_feature_count];
    for &s in &slots {
        let i = s as usize;
        if i < rendered_slots.len() {
            rendered_slots[i] = true;
        }
    }

    // walk the index alongside the slot cursor so we keep (slot, feature)
    // pairs together (decode_geometry_at_slots loses slot identity).
    let iter = iter_feature_index(&geom_bytes).map_err(map_artifact_err)?;
    let coord_area = iter.coord_area();
    let mut paired: Vec<(u32, FeatureGeom)> = Vec::with_capacity(slots.len());
    let mut cursor = 0usize;
    for (slot_idx, entry) in iter.enumerate() {
        if cursor >= slots.len() {
            break;
        }
        let entry = entry.map_err(map_artifact_err)?;
        let slot_u32 = u32::try_from(slot_idx).map_err(|_| RuntimeError::InvalidManifest {
            reason: "render: slot index overflow".into(),
        })?;
        if slot_u32 != slots[cursor] {
            continue;
        }
        cursor += 1;
        let geom = decode_one_geom(coord_area, &entry).map_err(map_artifact_err)?;
        paired.push((
            slot_u32,
            FeatureGeom {
                user_id: entry.user_id,
                bbox: entry.bbox,
                geom,
            },
        ));
    }

    let projected = if same_crs {
        paired
    } else {
        project_paired_features(paired, &binding.native_crs, &plan.crs)?
    };
    let mut ops = Vec::with_capacity(projected.len());
    for (slot, f) in projected {
        let style = class
            .as_ref()
            .and_then(|c| c.style_ref_for(slot))
            .and_then(|name| stylesheet.geometry.get(name).cloned())
            .unwrap_or_else(|| fallback.clone());
        if let Some(op) = feature_to_drawop(&f.geom, plan.bbox, plan.width, plan.height, style) {
            ops.push(op);
        }
    }
    Ok(DecodedPage {
        ops,
        rendered_slots,
        class,
    })
}

fn bbox_native(viewport: Bbox, from: &mars_types::CrsCode, to: &mars_types::CrsCode) -> Result<Bbox, RuntimeError> {
    planning::reproject_viewport(viewport, from, to)
}

fn bbox_to_f32(b: Bbox) -> [f32; 4] {
    [b.min_x as f32, b.min_y as f32, b.max_x as f32, b.max_y as f32]
}

fn project_paired_features(
    features: Vec<(u32, mars_artifact::FeatureGeom)>,
    from: &mars_types::CrsCode,
    to: &mars_types::CrsCode,
) -> Result<Vec<(u32, mars_artifact::FeatureGeom)>, RuntimeError> {
    let xform = mars_proj::cached_transformer(from, to).map_err(map_proj_err)?;
    let mut out = Vec::with_capacity(features.len());
    for (slot, f) in features {
        let geom = project_geom(&f.geom, &xform)?;
        out.push((
            slot,
            mars_artifact::FeatureGeom {
                user_id: f.user_id,
                bbox: f.bbox,
                geom,
            },
        ));
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
            subpaths: rings.iter().map(|r| ring_to_subpath(r, viewport, w, h, true)).collect(),
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

/// label candidate that has been resolved against the active stylesheet and
/// projected into request-CRS pixel space. carries enough state for the
/// collision pass to keep or drop it without redoing the projection.
struct PreparedLabel {
    anchor_px: (f32, f32),
    text: String,
    style: Arc<LabelStyle>,
    priority: u16,
    bbox_px: (f32, f32, f32, f32),
}

#[allow(clippy::too_many_arguments)]
fn prepare_labels(
    bytes: Bytes,
    plan: &RenderPlan,
    binding: &BindingMetadata,
    class: Option<&ClassResolver>,
    stylesheet: &Stylesheet,
    same_crs: bool,
    survival_filter: Option<&[bool]>,
    renderer: &dyn Renderer,
) -> Result<Vec<PreparedLabel>, RuntimeError> {
    let reader = ArtifactReader::open(bytes).map_err(map_artifact_err)?;
    let label_bytes = reader.section(SectionKind::LabelCandidates).map_err(map_artifact_err)?;
    let candidates = decode_label_candidates(&label_bytes).map_err(map_artifact_err)?;
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(candidates.len());
    let xform = if same_crs {
        None
    } else {
        Some(mars_proj::cached_transformer(&binding.native_crs, &plan.crs).map_err(map_proj_err)?)
    };
    for c in candidates {
        // FollowGeometry: drop slot-bearing candidates whose feature wasn't
        // rendered at this scale. slotless (pruned-feature) labels are
        // emitted unconditionally - they exist precisely because their
        // geometry was filtered out at compile time. compiler is the
        // primary enforcer; runtime stays defensive against drift (eg. an
        // older sidecar epoch left over after a swap).
        if let (Some(allow), Some(idx)) = (survival_filter, c.feature_idx) {
            let i = idx as usize;
            if i >= allow.len() || !allow[i] {
                continue;
            }
        }
        let anchor_world = match label_anchor_world(&c, xform.as_deref()) {
            Some(a) => a,
            None => continue,
        };
        let anchor_px = world_to_pixel(anchor_world, plan.bbox, plan.width, plan.height);
        if !inside_pixel_canvas(anchor_px, plan.width, plan.height) {
            continue;
        }
        let style_name = class
            .and_then(|cl| cl.style_refs().get(c.style_ref_idx as usize))
            .map(String::as_str);
        let Some(style) = style_name.and_then(|n| stylesheet.labels.get(n).cloned()) else {
            continue;
        };
        let bbox_px = match renderer.measure_text(&c.text, &style) {
            Ok(m) => text_bbox_from_metrics(anchor_px, m),
            // font lookup / shaping failure: drop the candidate. matches the
            // existing "drop on error" behaviour of style/anchor resolution
            // a few lines above.
            Err(_) => continue,
        };
        out.push(PreparedLabel {
            anchor_px,
            text: c.text,
            style,
            priority: c.priority,
            bbox_px,
        });
    }
    Ok(out)
}

fn label_anchor_world(c: &LabelCandidate, xform: Option<&mars_proj::Transformer>) -> Option<(f64, f64)> {
    let (wx, wy) = match &c.shape {
        LabelShape::Point { x, y } | LabelShape::PolygonAnchor { x, y } => (f64::from(*x), f64::from(*y)),
        // polyline labels: take the midpoint of the polyline. naive but
        // reasonable for v1; a future pass that reuses the placement
        // engine's arc-length sampling can refine.
        LabelShape::Polyline(points) => {
            if points.is_empty() {
                return None;
            }
            let mid = points[points.len() / 2];
            (f64::from(mid.0), f64::from(mid.1))
        }
    };
    match xform {
        None => Some((wx, wy)),
        Some(t) => t.transform_point(wx, wy).ok(),
    }
}

fn inside_pixel_canvas(p: (f32, f32), w: u32, h: u32) -> bool {
    p.0 >= 0.0 && p.1 >= 0.0 && p.0 <= w as f32 && p.1 <= h as f32
}

/// build a pixel-space bbox around `anchor` from the font-aware metrics the
/// renderer would use to rasterise the same run. anchor is the baseline
/// origin; bbox extends by half advance horizontally and by ascent / descent
/// vertically. centred horizontally because draw_label paints around the
/// anchor; the vertical extent uses the actual font ascent + descent so the
/// collision bbox matches what tiny-skia paints.
fn text_bbox_from_metrics(anchor: (f32, f32), m: mars_render_port::TextMetrics) -> (f32, f32, f32, f32) {
    let half_w = m.advance_x * 0.5;
    (
        anchor.0 - half_w,
        anchor.1 - m.ascent,
        anchor.0 + half_w,
        anchor.1 + m.descent,
    )
}

/// run a greedy collision pass over the accumulated label set and return
/// the surviving `DrawOp::Label` ops in placement order.
fn collide_and_emit_labels(mut labels: Vec<PreparedLabel>, _w: u32, _h: u32) -> Vec<DrawOp> {
    if labels.is_empty() {
        return Vec::new();
    }
    // priority desc → place high-priority labels first, drop conflicts.
    labels.sort_by_key(|l| std::cmp::Reverse(l.priority));
    let mut placed: Vec<(f32, f32, f32, f32)> = Vec::with_capacity(labels.len());
    let mut ops = Vec::with_capacity(labels.len());
    for label in labels {
        if placed.iter().any(|b| pixel_bbox_overlaps(*b, label.bbox_px)) {
            continue;
        }
        placed.push(label.bbox_px);
        ops.push(DrawOp::Label {
            anchor: label.anchor_px,
            text: label.text,
            style: label.style,
        });
    }
    ops
}

fn pixel_bbox_overlaps(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
    a.0 < b.2 && a.2 > b.0 && a.1 < b.3 && a.3 > b.1
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
        let geom = GeomKind::Polygon(vec![vec![
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (0.0, 10.0),
            (0.0, 0.0),
        ]]);
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
