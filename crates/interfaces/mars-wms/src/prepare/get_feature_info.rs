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

use super::viewport::resolve_viewport;
use super::ParsedGetFeatureInfo;
use crate::feature_info::info_format_mime;
use crate::{InfoFormat, WmsConfig, WmsError, MAX_FEATURE_COUNT};

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
    p: ParsedGetFeatureInfo,
    cfg: &WmsConfig,
) -> Result<ResolvedGetFeatureInfo, WmsError> {
    let mut plan = resolve_viewport(&p.viewport, cfg)?;

    let query_layers = resolve_query_layers(p.query_layers, &plan.layers)?;
    plan.layers = query_layers;

    let i = p.i.ok_or(WmsError::MissingParam("i"))?;
    let j = p.j.ok_or(WmsError::MissingParam("j"))?;
    if i >= plan.width || j >= plan.height {
        return Err(WmsError::InvalidParam {
            name: "i|j",
            reason: format!("({i},{j}) outside viewport {}x{}", plan.width, plan.height),
        });
    }

    let info_format_raw = p
        .info_format
        .as_deref()
        .ok_or(WmsError::MissingParam("info_format"))?;
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

fn resolve_query_layers(query_layers: Option<Vec<LayerId>>, layers: &[LayerId]) -> Result<Vec<LayerId>, WmsError> {
    let q = query_layers.ok_or(WmsError::MissingParam("query_layers"))?;
    if q.is_empty() {
        return Err(WmsError::InvalidParam {
            name: "query_layers",
            reason: "no layer names".into(),
        });
    }
    for ql in &q {
        if !layers.iter().any(|l| l == ql) {
            return Err(WmsError::InvalidParam {
                name: "query_layers",
                reason: format!("`{}` is not in LAYERS", ql.as_str()),
            });
        }
    }
    Ok(q)
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
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use mars_types::{CrsCode, ImageFormat, LayerId};

    use super::super::viewport::ParsedViewport;
    use super::*;

    fn cfg() -> WmsConfig {
        WmsConfig {
            allowlist_crs: vec![CrsCode::new("EPSG:25832")],
            formats: vec![ImageFormat::Png],
            max_image_dimension: 8192,
            max_pixels: 16_000_000,
            max_layers: 100,
            max_bbox_coord: 1e9,
            scale_pixel_size_m: 0.0254 / 96.0,
        }
    }

    fn parsed(query_layers: Vec<&str>, i: u32, j: u32, info_format: &str, feature_count: Option<u32>) -> ParsedGetFeatureInfo {
        ParsedGetFeatureInfo {
            viewport: ParsedViewport {
                version: Some("1.3.0".into()),
                layers: Some(vec![LayerId::new("a"), LayerId::new("b")]),
                crs: Some("EPSG:25832".into()),
                bbox: Some("0,0,100,100".into()),
                width: Some(10),
                height: Some(10),
                format: Some("image/png".into()),
                dpi: None,
            },
            query_layers: Some(query_layers.into_iter().map(LayerId::new).collect()),
            i: Some(i),
            j: Some(j),
            info_format: Some(info_format.into()),
            feature_count,
        }
    }

    #[test]
    fn query_layers_swapped_into_plan_layers() {
        // load-bearing semantic: plan.layers must end up = query_layers,
        // not the original LAYERS list, so the runtime walks only the
        // query subset.
        let p = parsed(vec!["a"], 5, 7, "text/plain", None);
        let r = resolve_get_feature_info(p, &cfg()).unwrap();
        assert_eq!(r.plan.layers.len(), 1);
        assert_eq!(r.plan.layers[0].as_str(), "a");
        assert_eq!(r.i, 5);
        assert_eq!(r.j, 7);
        assert_eq!(r.info_format, InfoFormat::TextPlain);
        assert_eq!(r.feature_count, 1);
    }

    #[test]
    fn query_layers_must_be_subset_of_layers() {
        let p = parsed(vec!["z"], 0, 0, "text/plain", None);
        let err = resolve_get_feature_info(p, &cfg()).unwrap_err();
        assert!(matches!(err, WmsError::InvalidParam { name: "query_layers", .. }));
    }

    #[test]
    fn missing_query_layers_reports_missing() {
        let mut p = parsed(vec!["a"], 0, 0, "text/plain", None);
        p.query_layers = None;
        let err = resolve_get_feature_info(p, &cfg()).unwrap_err();
        assert!(matches!(err, WmsError::MissingParam("query_layers")));
    }

    #[test]
    fn pixel_out_of_viewport_rejected() {
        let p = parsed(vec!["a"], 10, 0, "text/plain", None);
        let err = resolve_get_feature_info(p, &cfg()).unwrap_err();
        assert!(matches!(err, WmsError::InvalidParam { name: "i|j", .. }));
    }

    #[test]
    fn unsupported_info_format_rejected() {
        let p = parsed(vec!["a"], 0, 0, "application/vnd.ogc.gml", None);
        let err = resolve_get_feature_info(p, &cfg()).unwrap_err();
        assert!(matches!(err, WmsError::InvalidParam { name: "info_format", .. }));
    }

    #[test]
    fn feature_count_default_is_one() {
        let p = parsed(vec!["a"], 0, 0, "text/plain", None);
        let r = resolve_get_feature_info(p, &cfg()).unwrap();
        assert_eq!(r.feature_count, 1);
    }

    #[test]
    fn feature_count_clamped_to_max() {
        let p = parsed(vec!["a"], 0, 0, "text/plain", Some(MAX_FEATURE_COUNT + 100));
        let r = resolve_get_feature_info(p, &cfg()).unwrap();
        assert_eq!(r.feature_count, MAX_FEATURE_COUNT);
    }

    #[test]
    fn feature_count_zero_rejected() {
        let p = parsed(vec!["a"], 0, 0, "text/plain", Some(0));
        let err = resolve_get_feature_info(p, &cfg()).unwrap_err();
        assert!(matches!(err, WmsError::InvalidParam { name: "feature_count", .. }));
    }
}
