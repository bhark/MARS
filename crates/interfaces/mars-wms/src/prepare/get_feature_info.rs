//! GetFeatureInfo normalisation: takes a [`super::ParsedGetFeatureInfo`]
//! and produces a validated [`ResolvedGetFeatureInfo`]. Reuses
//! [`super::viewport::resolve_viewport`] so allowlist + bound + axis-order
//! checks stay single-source with GetMap.
//!
//! semantic note: `query_layers` (when validated as a subset of `layers`)
//! is swapped into `plan.layers` so the runtime walks exactly those
//! bindings. losing that swap silently re-renders the full set; the test
//! `query_layers_swapped_into_plan_layers` guards against regression.

use mars_runtime::RenderPlan;
use mars_types::LayerId;

use super::ParsedGetFeatureInfo;
use super::viewport::resolve_viewport;
use crate::feature_info::info_format_mime;
use mars_config::ServiceOp;

use crate::{InfoFormat, MAX_FEATURE_COUNT, WmsConfig, WmsError, WmsVersion};

/// Fully-validated GetFeatureInfo request. `plan.layers` has been swapped
/// to QUERY_LAYERS so the runtime walks only those bindings.
#[derive(Debug, Clone)]
pub struct ResolvedGetFeatureInfo {
    /// Render plan with `layers` already restricted to `query_layers`.
    pub plan: RenderPlan,
    /// Pixel-space x coordinate of the click (origin top-left).
    pub i: u32,
    /// Pixel-space y coordinate of the click (origin top-left).
    pub j: u32,
    /// Negotiated info-format.
    pub info_format: InfoFormat,
    /// Maximum feature hits to return; spec default 1, capped at
    /// [`MAX_FEATURE_COUNT`].
    pub feature_count: u32,
}

pub(crate) fn resolve_get_feature_info(
    mut p: ParsedGetFeatureInfo,
    cfg: &WmsConfig,
    version: WmsVersion,
) -> Result<ResolvedGetFeatureInfo, WmsError> {
    // silently drop gfi-denied entries from LAYERS so resolve_viewport's
    // per-op gate (called with GetFeatureInfo here) doesn't error on them.
    // empty post-filter => OperationNotPermitted on the first denied layer.
    if let Some(layers) = p.viewport.layers.as_mut() {
        let first_denied = layers
            .iter()
            .find(|l| !cfg.permits(l, ServiceOp::WmsGetFeatureInfo))
            .cloned();
        layers.retain(|l| cfg.permits(l, ServiceOp::WmsGetFeatureInfo));
        if layers.is_empty()
            && let Some(layer) = first_denied
        {
            return Err(WmsError::OperationNotPermitted {
                layer,
                op: ServiceOp::WmsGetFeatureInfo,
            });
        }
    }

    let mut plan = resolve_viewport(&p.viewport, cfg, version, ServiceOp::WmsGetFeatureInfo)?;

    let query_layers = resolve_query_layers(p.query_layers, &plan.layers, cfg)?;
    plan.layers = query_layers;

    let i = p.i.ok_or(WmsError::MissingParam("i"))?;
    let j = p.j.ok_or(WmsError::MissingParam("j"))?;
    if i >= plan.width || j >= plan.height {
        return Err(WmsError::InvalidParam {
            name: "i|j",
            reason: format!("({i},{j}) outside viewport {}x{}", plan.width, plan.height),
        });
    }

    let info_format_raw = p.info_format.as_deref().ok_or(WmsError::MissingParam("info_format"))?;
    let info_format = info_format_mime(info_format_raw).ok_or(WmsError::InvalidParam {
        name: "info_format",
        reason: format!("unsupported `{info_format_raw}`"),
    })?;

    let feature_count = resolve_feature_count(p.feature_count)?;

    Ok(ResolvedGetFeatureInfo {
        plan,
        i,
        j,
        info_format,
        feature_count,
    })
}

fn resolve_query_layers(
    query_layers: Option<Vec<LayerId>>,
    layers: &[LayerId],
    cfg: &WmsConfig,
) -> Result<Vec<LayerId>, WmsError> {
    let q = query_layers.ok_or(WmsError::MissingParam("query_layers"))?;
    if q.is_empty() {
        return Err(WmsError::InvalidParam {
            name: "query_layers",
            reason: "no layer names".into(),
        });
    }
    // silently drop gfi-denied entries first so the subset check below sees
    // the same filtered set the caller-side LAYERS filter applied. empty
    // post-filter (non-empty original) => OperationNotPermitted pinned to
    // the first originally-denied layer.
    let first_denied = q
        .iter()
        .find(|l| !cfg.permits(l, ServiceOp::WmsGetFeatureInfo))
        .cloned();
    let filtered: Vec<LayerId> = q
        .into_iter()
        .filter(|l| cfg.permits(l, ServiceOp::WmsGetFeatureInfo))
        .collect();
    if filtered.is_empty()
        && let Some(layer) = first_denied
    {
        return Err(WmsError::OperationNotPermitted {
            layer,
            op: ServiceOp::WmsGetFeatureInfo,
        });
    }
    for ql in &filtered {
        if !layers.iter().any(|l| l == ql) {
            return Err(WmsError::InvalidParam {
                name: "query_layers",
                reason: format!("`{}` is not in LAYERS", ql.as_str()),
            });
        }
    }
    Ok(filtered)
}

fn resolve_feature_count(opt: Option<u32>) -> Result<u32, WmsError> {
    let n = opt.unwrap_or(1);
    if n == 0 {
        return Err(WmsError::InvalidParam {
            name: "feature_count",
            reason: "must be >= 1".into(),
        });
    }
    Ok(n.min(MAX_FEATURE_COUNT))
}

#[cfg(test)]
mod tests;
