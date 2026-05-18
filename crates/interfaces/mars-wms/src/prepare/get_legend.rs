//! GetLegendGraphic normalisation: takes a [`super::ParsedGetLegend`] and
//! produces a validated [`ResolvedGetLegend`]. Thin today; the wrapper
//! exists so future SLD-style swatch defaults land in one place rather
//! than being grafted onto [`mars_runtime::LegendPlan`].

use mars_runtime::LegendPlan;
use mars_types::{ImageFormat, LayerId};

use super::ParsedGetLegend;
use mars_config::ServiceOp;

use crate::{WmsConfig, WmsError};

/// Fully-validated GetLegendGraphic request.
#[derive(Debug, Clone)]
pub struct ResolvedGetLegend {
    pub plan: LegendPlan,
}

pub(crate) fn resolve_get_legend(p: ParsedGetLegend, cfg: &WmsConfig) -> Result<ResolvedGetLegend, WmsError> {
    let layer = p.layer.ok_or(WmsError::MissingParam("layer"))?;
    let layer = LayerId::new(layer);
    if !cfg.permits(&layer, ServiceOp::WmsGetLegendGraphic) {
        return Err(WmsError::OperationNotPermitted {
            layer,
            op: ServiceOp::WmsGetLegendGraphic,
        });
    }

    let format_raw = p.format.as_deref().ok_or(WmsError::MissingParam("format"))?;
    let format = resolve_format(format_raw, cfg)?;

    let swatch_width = p.width.unwrap_or(LegendPlan::DEFAULT_SWATCH_WIDTH);
    let swatch_height = p.height.unwrap_or(LegendPlan::DEFAULT_SWATCH_HEIGHT);
    if swatch_width == 0 || swatch_height == 0 {
        return Err(WmsError::InvalidParam {
            name: "width|height",
            reason: "must be > 0".into(),
        });
    }
    if swatch_width > cfg.max_image_dimension || swatch_height > cfg.max_image_dimension {
        return Err(WmsError::InvalidParam {
            name: "width|height",
            reason: format!("max dimension is {}", cfg.max_image_dimension),
        });
    }

    Ok(ResolvedGetLegend {
        plan: LegendPlan {
            layer,
            format,
            swatch_width,
            swatch_height,
            rule: p.rule,
        },
    })
}

fn resolve_format(raw: &str, cfg: &WmsConfig) -> Result<ImageFormat, WmsError> {
    let format = ImageFormat::from_mime(raw).ok_or_else(|| WmsError::InvalidParam {
        name: "format",
        reason: format!("unsupported {raw}"),
    })?;
    if !cfg.formats.is_empty() && !cfg.formats.contains(&format) {
        return Err(WmsError::InvalidParam {
            name: "format",
            reason: format!("{raw} not enabled"),
        });
    }
    Ok(format)
}

#[cfg(test)]
mod tests;
