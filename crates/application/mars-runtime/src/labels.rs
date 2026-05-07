//! per-request label collision pass. SPEC §14.
//!
//! the path-draw loop produces geometry; this module reads each layer's
//! `LabelCandidates` section, projects each anchor into pixel space, and
//! greedily places candidates whose AABB does not intersect anything already
//! accepted. a single rtree is shared across all layers in the request.
//!
//! v1 emits a `DrawOp::Label` per accepted candidate at a baseline anchor
//! and lets the renderer adapter shape / rasterise the glyphs. text
//! footprint is computed here (via `mars-text::measure`) so the collision
//! pass and the renderer agree on extent.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use mars_artifact::{ArtifactReader, LabelShape, SectionKind, decode_label_candidates, decode_style_refs};
use mars_observability::Metrics;
use mars_proj::Transformer;
use mars_render_port::DrawOp;
use mars_style::{LabelStyle, Stylesheet};
use mars_text::Fonts;
use mars_types::{Bbox, LayerId};
use rstar::{AABB, RTree};

use crate::RuntimeError;
use crate::draw::Viewport;

/// per-request inputs for the collision pass.
pub(crate) struct LabelInputs<'a> {
    pub layers: &'a [(LayerId, ArtifactReader)],
    pub stylesheet: &'a Stylesheet,
    pub viewport: Viewport,
    pub canonical_bbox: Bbox,
    pub reproject: Option<&'a Transformer>,
    pub fonts: &'a Fonts,
    pub metrics: &'a Metrics,
}

/// drive the collision pass and append `DrawOp::Label` ops for accepted
/// candidates. errors propagate; rejecting a single candidate is silent.
pub(crate) fn collide_and_emit(input: &LabelInputs<'_>, out: &mut Vec<DrawOp>) -> Result<(), RuntimeError> {
    let started = Instant::now();
    let mut prepared = Vec::new();
    for (layer_idx, (_layer, art)) in input.layers.iter().enumerate() {
        match prepare_layer(layer_idx, art, input)? {
            Some(items) => prepared.extend(items),
            None => continue,
        }
    }

    // deterministic ordering. priority high → low; ties broken by feature_id,
    // foreign_origin (primary before foreign), text bytes; never on f32.
    prepared.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then(a.layer_idx.cmp(&b.layer_idx))
            .then(a.feature_id.cmp(&b.feature_id))
            .then(a.foreign_origin.cmp(&b.foreign_origin))
            .then(a.text.as_bytes().cmp(b.text.as_bytes()))
    });

    let mut tree: RTree<rstar::primitives::Rectangle<[f32; 2]>> = RTree::new();
    let mut accepted_keys: HashSet<(usize, u64)> = HashSet::new();
    for cand in prepared {
        // per-feature dedup keyed by (layer, feature_id) so identical primary
        // and foreign candidates collapse to the primary.
        if !accepted_keys.insert((cand.layer_idx, cand.feature_id)) {
            continue;
        }
        let env = AABB::from_corners(cand.aabb.0, cand.aabb.1);
        let conflict = tree.locate_in_envelope_intersecting(&env).next().is_some();
        if conflict {
            continue;
        }
        tree.insert(rstar::primitives::Rectangle::from_corners(cand.aabb.0, cand.aabb.1));
        out.push(DrawOp::Label {
            anchor: cand.anchor,
            text: cand.text,
            style: cand.style,
        });
    }

    input.metrics.observe_label_seconds(started.elapsed());
    Ok(())
}

struct PreparedCandidate {
    priority: u16,
    layer_idx: usize,
    feature_id: u64,
    foreign_origin: bool,
    text: String,
    style: Arc<LabelStyle>,
    anchor: (f32, f32),
    aabb: ([f32; 2], [f32; 2]),
}

fn prepare_layer(
    layer_idx: usize,
    art: &ArtifactReader,
    input: &LabelInputs<'_>,
) -> Result<Option<Vec<PreparedCandidate>>, RuntimeError> {
    let candidates_section = match art.section(SectionKind::LabelCandidates) {
        Ok(b) => b,
        Err(mars_artifact::ArtifactError::SectionMissing(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let style_refs_section = art.section(SectionKind::StyleRefs)?;
    let style_refs = decode_style_refs(&style_refs_section)?;
    let candidates = decode_label_candidates(&candidates_section)?;
    if candidates.is_empty() {
        return Ok(None);
    }

    // resolve label styles by ref index once per layer; the per-candidate loop
    // becomes a Vec index instead of a BTreeMap-by-String probe.
    let resolved_styles: Vec<Option<Arc<LabelStyle>>> = style_refs
        .iter()
        .map(|name| input.stylesheet.labels.get(name).cloned())
        .collect();

    let mut out = Vec::with_capacity(candidates.len());
    for cand in candidates {
        let style = match resolved_styles.get(cand.style_ref_idx as usize) {
            Some(Some(s)) => s,
            Some(None) => {
                tracing::debug!(
                    idx = cand.style_ref_idx,
                    name = %style_refs.get(cand.style_ref_idx as usize).map(String::as_str).unwrap_or(""),
                    "label style not in stylesheet"
                );
                continue;
            }
            None => {
                tracing::debug!(idx = cand.style_ref_idx, "label style_ref_idx out of range");
                continue;
            }
        };
        let Some((world_x, world_y)) = candidate_anchor(&cand.shape) else {
            continue;
        };

        // canonical-bbox cull keeps neighbour-cell labels out when they cannot
        // visibly fall in the viewport; cheap enough to do before reproject.
        if !point_in_bbox(world_x as f64, world_y as f64, input.canonical_bbox) {
            continue;
        }

        let (rx, ry) = match input.reproject {
            Some(t) => t.transform_point(world_x as f64, world_y as f64)?,
            None => (world_x as f64, world_y as f64),
        };
        let (px, py) = input.viewport.project(rx, ry);

        // measure once per candidate. mars-text caches behind the database;
        // re-shaping each time is acceptable for v1 candidate counts.
        let run = mars_text::measure(&cand.text, style.as_ref(), input.fonts)
            .map_err(|e| RuntimeError::Render(mars_render_port::RenderError::Backend(format!("font measure: {e}"))))?;

        let halo_w = style.halo.as_ref().map(|h| h.width).unwrap_or(0.0);
        let pad = halo_w + style.min_distance;
        // anchor is the baseline lower-left; centre horizontally on the
        // geometry anchor and vertical-centre between ascent/descent so the
        // visual centre lands on the placement point.
        let anchor_x = px - run.advance_x * 0.5;
        let anchor_y = py + (run.ascent - run.descent) * 0.5;
        let min = [anchor_x - pad, anchor_y - run.ascent - pad];
        let max = [anchor_x + run.advance_x + pad, anchor_y + run.descent + pad];

        out.push(PreparedCandidate {
            priority: cand.priority,
            layer_idx,
            feature_id: cand.feature_id,
            foreign_origin: cand.foreign_origin,
            text: cand.text,
            style: Arc::clone(style),
            anchor: (anchor_x, anchor_y),
            aabb: (min, max),
        });
    }
    Ok(Some(out))
}

fn candidate_anchor(shape: &LabelShape) -> Option<(f32, f32)> {
    match shape {
        LabelShape::Point { x, y } | LabelShape::PolygonAnchor { x, y } => Some((*x, *y)),
        LabelShape::Polyline(verts) => {
            // line labels: take the mid-vertex as anchor. arc-length sampling
            // along the polyline is reserved for v1.1.
            verts.get(verts.len() / 2).copied()
        }
    }
}

fn point_in_bbox(x: f64, y: f64, bbox: Bbox) -> bool {
    x >= bbox.min_x && x <= bbox.max_x && y >= bbox.min_y && y <= bbox.max_y
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use mars_render_port::DrawOp;
    use mars_style::{Colour, LabelStyle};

    use super::*;

    fn style(priority: u16) -> LabelStyle {
        LabelStyle {
            font_family: "DejaVu Sans".into(),
            font_size: 14.0,
            fill: Colour::rgba(0, 0, 0, 0xff),
            halo: None,
            priority,
            min_distance: 0.0,
        }
    }

    fn rect(min: [f32; 2], max: [f32; 2]) -> ([f32; 2], [f32; 2]) {
        (min, max)
    }

    #[test]
    fn greedy_places_higher_priority_first() {
        // three overlapping point candidates, decreasing priority. the high-
        // priority one wins; lower ones are rejected.
        let mut tree: RTree<rstar::primitives::Rectangle<[f32; 2]>> = RTree::new();
        let aabbs = [
            (3i32, rect([0.0, 0.0], [10.0, 10.0])),
            (2i32, rect([1.0, 1.0], [11.0, 11.0])),
            (1i32, rect([2.0, 2.0], [12.0, 12.0])),
        ];
        let mut accepted = vec![];
        let mut sorted = aabbs.to_vec();
        sorted.sort_by_key(|b| std::cmp::Reverse(b.0));
        for (p, r) in sorted {
            let env = AABB::from_corners(r.0, r.1);
            if tree.locate_in_envelope_intersecting(&env).next().is_some() {
                continue;
            }
            tree.insert(rstar::primitives::Rectangle::from_corners(r.0, r.1));
            accepted.push(p);
        }
        assert_eq!(accepted, vec![3]);
    }

    #[test]
    fn foreign_origin_dedup_keeps_primary() {
        // simulate: same feature_id appears twice — once primary (foreign=false)
        // once as foreign cell origin (foreign=true). after sort by foreign asc,
        // primary lands first; dedup discards the second. emulate just the
        // dedup step here (the rtree work is covered above).
        let mut accepted: HashSet<(usize, u64)> = HashSet::new();
        let mut emitted = vec![];
        let cands = vec![(false, 7u64, "primary"), (true, 7u64, "foreign")];
        for (foreign, fid, _txt) in cands {
            // sort key guarantees primary (foreign=false) sorts first
            let key = (0usize, fid);
            if accepted.insert(key) {
                emitted.push(foreign);
            }
        }
        assert_eq!(emitted, vec![false], "primary must win, foreign discarded");
    }

    #[test]
    fn drawop_label_arc_is_cheap_to_share() {
        // smoke check that the new shape compiles + cloning is cheap.
        let s = Arc::new(style(1));
        let op = DrawOp::Label {
            anchor: (1.0, 2.0),
            text: "hi".into(),
            style: s.clone(),
        };
        match op {
            DrawOp::Label {
                ref text, ref style, ..
            } => {
                assert_eq!(text, "hi");
                assert_eq!(style.priority, 1);
            }
            DrawOp::Path { .. } => unreachable!(),
        }
    }
}
