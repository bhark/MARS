//! label pipeline: candidate decode, projection, collision, emission.
//!
//! self-contained slice of the render path. `prepare_labels` opens a label
//! sidecar and produces `PreparedLabel`s already projected into request-CRS
//! pixel space; `collide_and_emit_labels` runs the priority-ordered greedy
//! collision pass over the union of all layers' candidates and emits the
//! surviving `DrawOp::Label` ops in placement order.

// re-exports below are `pub` so the `bench-internals` feature can lift them
// out via `mars_runtime::bench_internals`. without the feature they ride
// behind `pub(crate) mod label` and never escape the crate.
#![allow(unreachable_pub)]

use std::sync::Arc;

use bytes::Bytes;
use mars_artifact::{ArtifactReader, LabelShape, SectionKind, decode_label_candidates};
use mars_render_port::Renderer;
use mars_style::{Placement, Stylesheet};
use mars_types::BindingMetadata;

use crate::{RenderPlan, RuntimeError};

use super::decode::ClassResolver;
use super::project::world_to_pixel;
use super::{map_artifact_err, map_proj_err};

mod candidate;
mod collision;
mod geometry;
mod polyline;
mod position;
mod projection;

#[cfg(test)]
mod tests;

pub use candidate::PreparedLabel;
pub use collision::collide_and_emit_labels;

#[cfg(feature = "bench-internals")]
pub use candidate::{PositionCandidate, PreparedPlacement, new_position_candidate, new_prepared_label};

use geometry::{effective_angle_rad, filter_for_partials, inside_pixel_canvas, label_anchor_world};
use polyline::sample_polyline_labels;
use position::build_placement;

#[allow(clippy::too_many_arguments)]
pub(in crate::render) fn prepare_labels(
    bytes: Bytes,
    plan: &RenderPlan,
    binding: &BindingMetadata,
    class: Option<&ClassResolver>,
    stylesheet: &Stylesheet,
    same_crs: bool,
    survival_filter: Option<&[bool]>,
    placement: &Placement,
    renderer: &dyn Renderer,
    denom: u64,
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
        let style_name = class
            .and_then(|cl| cl.style_refs().get(c.style_ref_idx as usize))
            .map(String::as_str);
        // resolve the authored LabelStyle to a renderer-facing form once per
        // candidate: every downstream measure/place call consumes the
        // resolved variant.
        let Some(style) = style_name
            .and_then(|n| stylesheet.labels.get(n))
            .map(|s| Arc::new(s.resolve(denom)))
        else {
            continue;
        };
        // polyline candidates expand into multiple samples along the line
        // when the layer placement is Placement::Line; otherwise fall through
        // to the historical midpoint anchor.
        if let (
            LabelShape::Polyline(points),
            Placement::Line {
                repeat_m,
                max_angle_delta_deg,
                angle_mode,
            },
        ) = (&c.shape, placement)
        {
            sample_polyline_labels(
                points,
                xform.as_deref(),
                plan,
                *repeat_m,
                *max_angle_delta_deg,
                *angle_mode,
                &c.text,
                c.priority,
                &style,
                renderer,
                &mut out,
            );
            continue;
        }
        let anchor_world = match label_anchor_world(&c, xform.as_deref()) {
            Some(a) => a,
            None => continue,
        };
        let raw_anchor_px = world_to_pixel(anchor_world, plan.bbox, plan.width, plan.height);
        if !inside_pixel_canvas(raw_anchor_px, plan.width, plan.height) {
            continue;
        }
        let metrics = match renderer.measure_text(&c.text, &style) {
            Ok(m) => m,
            // font lookup / shaping failure: drop the candidate. matches the
            // existing "drop on error" behaviour of style/anchor resolution
            // a few lines above.
            Err(_) => continue,
        };
        // angle resolution: explicit numeric ANGLE wins over the
        // placement-derived angle (which is zero for the point/polygon path).
        let angle_rad = effective_angle_rad(&style, 0.0);
        let placement = build_placement(raw_anchor_px, &metrics, &style, angle_rad);
        let Some(placement) = filter_for_partials(placement, style.partials, plan.width, plan.height) else {
            continue;
        };
        out.push(PreparedLabel {
            raw_anchor_px,
            text: c.text,
            style,
            priority: c.priority,
            angle_rad,
            placement,
        });
    }
    Ok(out)
}
