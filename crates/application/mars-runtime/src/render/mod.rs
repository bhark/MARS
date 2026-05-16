//! per-request render orchestration.
//!
//! pulls the threads of `state` (binding/level/page index), `plan` (level
//! pick and viewport intersection), and `fetch` (page bytes via the cache
//! port) together into the actual render pipeline that `Runtime::render`
//! exposes. features whose class chain resolves to no stylesheet entry are
//! dropped (counted via `mars_render_feature_unstyled_total`); a class that
//! names a missing stylesheet entry surfaces as `RuntimeError::StylesheetDrift`.
//!
//! tracing target: `mars_runtime::render`. spans: `render.plan`,
//! `render.layer`, `render.layer.fetch`, `render.layer.decode`,
//! `render.collide`, `render.paint`, `render.encode`. enable with
//! `RUST_LOG=mars_runtime::render=info`.

mod decode;
// `pub(crate)` so the `bench-internals` feature can re-export the collision
// API. without the feature this is effectively private (parent `render`
// module is itself `mod`, not `pub mod`).
pub(crate) mod label;
mod marker;
mod project;
mod raster;

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use mars_config::{Layer, ScaleWindow};
use mars_render_port::{Canvas, DrawOp, Renderer};
use mars_style::{LabelSurvival, LayerGeomKind, Placement, default_placement};
use mars_types::{BindingMetadata, LayerId, PageEntry};
use tracing::{Instrument, info_span};

use crate::state::RuntimeState;
use crate::{Deps, RenderPlan, RuntimeError};
use crate::{fetch::fetch_page, fetch::fetch_sidecar, plan as planning};

use decode::{DecodedPage, decode_page_to_ops};
use label::{PreparedLabel, collide_and_emit_labels, prepare_labels};

/// drive one render plan end-to-end. produces encoded image bytes ready to
/// hand back to the WMS / WMTS interface.
pub(crate) async fn render_plan(state: &RuntimeState, deps: &Deps, plan: &RenderPlan) -> Result<Vec<u8>, RuntimeError> {
    let span = info_span!(
        "render.plan",
        width = plan.width,
        height = plan.height,
        layers = plan.layers.len(),
    );
    async move {
        let config = state.config_or_err()?;
        let page_fetch_concurrency = config.render.page_fetch_concurrency.max(1);
        let canvas = Canvas {
            width: plan.width,
            height: plan.height,
            background: None,
        };
        // ζ.1: overlap per-layer work via FuturesUnordered, then reassemble in
        // plan order so z-stacking and label collision priority stay
        // deterministic regardless of completion order.
        let mut futs: FuturesUnordered<_> = plan
            .layers
            .iter()
            .enumerate()
            .map(|(idx, layer_id)| render_one_layer(idx, layer_id, state, deps, plan, config, page_fetch_concurrency))
            .collect();
        let mut slots: Vec<Option<LayerOutput>> = (0..plan.layers.len()).map(|_| None).collect();
        while let Some(res) = futs.next().await {
            let (idx, out) = res?;
            slots[idx] = out;
        }
        drop(futs);

        let mut all_ops: Vec<DrawOp> = Vec::new();
        let mut all_labels: Vec<PreparedLabel> = Vec::new();
        for slot in slots.into_iter().flatten() {
            all_ops.extend(slot.ops);
            all_labels.extend(slot.labels);
        }

        // greedy collision over the accumulated label set: sort by priority
        // descending, place survivors that don't collide with already-placed
        // labels' approximate text bbox.
        let label_ops = info_span!("render.collide", n = all_labels.len())
            .in_scope(|| collide_and_emit_labels(all_labels, plan.width, plan.height));
        all_ops.extend(label_ops);

        let pixmap =
            info_span!("render.paint", ops = all_ops.len()).in_scope(|| deps.renderer.render(canvas, &all_ops))?;
        let bytes = info_span!("render.encode", format = ?plan.format)
            .in_scope(|| deps.encoder.encode(&pixmap, plan.format))?;
        Ok(bytes)
    }
    .instrument(span)
    .await
}

/// drive a single layer's pipeline (binding pick -> page resolve -> page
/// fetch+decode). returns `(idx, None)` when the layer has no binding for
/// this scale or no pages intersect the viewport. instrumented with the
/// per-layer `render.layer` span so concurrent layers produce overlapping
/// span entries when ζ.1's FuturesUnordered drives them.
async fn render_one_layer(
    idx: usize,
    layer_id: &LayerId,
    state: &RuntimeState,
    deps: &Deps,
    plan: &RenderPlan,
    config: &mars_config::Config,
    page_fetch_concurrency: usize,
) -> Result<(usize, Option<LayerOutput>), RuntimeError> {
    let layer_span = info_span!("render.layer", name = %layer_id);
    async move {
        let layer_cfg = lookup_layer(config, layer_id)?;
        if matches!(
            mars_style::LayerKind::parse(layer_cfg.kind.as_str()),
            Some(mars_style::LayerKind::Raster)
        ) {
            let ops = raster::render_raster_layer(state, deps, plan, layer_id, page_fetch_concurrency).await?;
            if ops.is_empty() {
                return Ok((idx, None));
            }
            return Ok((
                idx,
                Some(LayerOutput {
                    ops,
                    labels: Vec::new(),
                }),
            ));
        }
        let denom = crate::denom_from_plan(plan.bbox.width(), plan.width, plan.scale_pixel_size_m);
        let Some((binding_id, level)) =
            planning::pick_binding_and_level(layer_cfg, denom, plan.scale_pixel_size_m, state)
        else {
            return Ok::<(usize, Option<LayerOutput>), RuntimeError>((idx, None));
        };
        // per-class scale gating: classes with a `scale:` window only fire when
        // the request denom falls inside it. None means always-on. precompute
        // once per layer; the page loop reuses the mask via class_idx.
        let class_active = class_active_mask(layer_cfg, denom);
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
            return Ok((idx, None));
        }
        let placement = resolve_layer_placement(layer_cfg);
        let out = render_layer_pages(
            deps,
            state,
            layer_id,
            binding,
            &pages,
            plan,
            layer_cfg.label_survival,
            &placement,
            deps.renderer.as_ref(),
            page_fetch_concurrency,
            &class_active,
            denom,
        )
        .await?;
        Ok((idx, Some(out)))
    }
    .instrument(layer_span)
    .await
}

/// resolve a layer's label placement: explicit when set, otherwise the
/// geometry-kind default. unknown geometry kinds fall through to
/// `Placement::Point` so we never panic on misconfigured config.
fn resolve_layer_placement(layer: &Layer) -> Placement {
    if let Some(p) = layer.label.as_ref().and_then(|l| l.placement.clone()) {
        return p;
    }
    match LayerGeomKind::parse(&layer.kind) {
        Some(k) => default_placement(k),
        None => Placement::Point,
    }
}

/// per-class active mask at a given request denom. an entry is `true` when
/// the class either has no `scale:` window or its half-open `[min, max)`
/// covers `denom`. mirrors MapServer CLASS MIN/MAXSCALEDENOM semantics.
fn class_active_mask(layer: &Layer, denom: u32) -> Vec<bool> {
    layer
        .classes
        .iter()
        .map(|c| match &c.scale {
            None => true,
            Some(s) => scale_window_contains(s, denom),
        })
        .collect()
}

fn scale_window_contains(s: &ScaleWindow, denom: u32) -> bool {
    let d = u64::from(denom);
    s.min.is_none_or(|m| d >= m) && s.max.is_none_or(|m| d < m)
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
    label_survival: LabelSurvival,
    placement: &Placement,
    renderer: &dyn Renderer,
    page_fetch_concurrency: usize,
    class_active: &[bool],
    denom: u32,
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
        let fetch_span = info_span!(
            "render.layer.fetch",
            name = %layer_id,
            page = %entry.key.page_id,
        );
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
        .instrument(fetch_span)
    });
    let mut stream = futures_util::stream::iter(fetches).buffered(page_fetch_concurrency);

    let mut out = LayerOutput {
        ops: Vec::new(),
        labels: Vec::new(),
    };
    let same_crs = binding.native_crs.as_str() == plan.crs.as_str();
    while let Some(res) = stream.next().await {
        let (entry, page_bytes, class_bytes, label_bytes) = res?;
        let decode_span = info_span!(
            "render.layer.decode",
            name = %layer_id,
            page = %entry.key.page_id,
        );
        let _decode_enter = decode_span.enter();
        let DecodedPage {
            ops: mut page_ops,
            rendered_slots,
            class,
            unstyled_count,
        } = decode_page_to_ops(
            page_bytes,
            class_bytes,
            &entry,
            plan,
            binding,
            layer_id,
            &state.stylesheet,
            same_crs,
            class_active,
            u64::from(denom),
        )?;
        if unstyled_count > 0 {
            deps.metrics
                .inc_render_feature_unstyled(layer_id.as_str(), unstyled_count);
        }
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
                placement,
                renderer,
                u64::from(denom),
            )?;
            out.labels.append(&mut prepared);
        }
    }
    Ok(out)
}

pub(super) fn map_artifact_err(e: mars_artifact::ArtifactError) -> RuntimeError {
    RuntimeError::InvalidManifest {
        reason: format!("artifact decode error: {e}"),
    }
}

pub(super) fn map_proj_err(e: mars_proj::ProjError) -> RuntimeError {
    RuntimeError::InvalidManifest {
        reason: format!("projection error: {e}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn class_scale_window_gates_at_denom() {
        // half-open [25001, 100001): denom 25001 active, 100001 not.
        let s = ScaleWindow {
            min: Some(25_001),
            max: Some(100_001),
        };
        assert!(scale_window_contains(&s, 25_001));
        assert!(scale_window_contains(&s, 100_000));
        assert!(!scale_window_contains(&s, 25_000));
        assert!(!scale_window_contains(&s, 100_001));
    }

    #[test]
    fn class_scale_window_open_bounds() {
        let s_no_min = ScaleWindow {
            min: None,
            max: Some(50),
        };
        assert!(scale_window_contains(&s_no_min, 0));
        assert!(scale_window_contains(&s_no_min, 49));
        assert!(!scale_window_contains(&s_no_min, 50));

        let s_no_max = ScaleWindow {
            min: Some(50),
            max: None,
        };
        assert!(!scale_window_contains(&s_no_max, 49));
        assert!(scale_window_contains(&s_no_max, 1_000_000));
    }
}
