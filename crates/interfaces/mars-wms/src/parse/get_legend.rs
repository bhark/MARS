//! `GetLegendGraphic` KVP parsing per OGC SLD-WMS.

use mars_runtime::LegendPlan;
use mars_types::LayerId;

use super::common::{Kvp, parse_format, parse_kvp, require};
use crate::{WmsConfig, WmsError};

/// Parse a `GetLegendGraphic` query-string into a [`LegendPlan`].
pub fn parse_get_legend_graphic(query: &str, cfg: &WmsConfig) -> Result<LegendPlan, WmsError> {
    let kvp = parse_kvp(query);
    parse_get_legend_graphic_inner(&kvp, cfg)
}

pub(super) fn parse_get_legend_graphic_inner(kvp: &Kvp, cfg: &WmsConfig) -> Result<LegendPlan, WmsError> {
    let layer_raw = require(kvp, "layer")?;
    let layer = LayerId::new(layer_raw);

    let format_raw = require(kvp, "format")?;
    let format = parse_format(&format_raw)?;
    if !cfg.formats.is_empty() && !cfg.formats.contains(&format) {
        return Err(WmsError::InvalidParam {
            name: "format",
            reason: format!("{format_raw} not enabled"),
        });
    }

    // width/height per OGC SLD-WMS describe a *single* swatch dimension.
    // Optional; default 20x20 matches MapServer.
    let swatch_width = parse_optional_dim(kvp, "width")?.unwrap_or(LegendPlan::DEFAULT_SWATCH_WIDTH);
    let swatch_height = parse_optional_dim(kvp, "height")?.unwrap_or(LegendPlan::DEFAULT_SWATCH_HEIGHT);
    if swatch_width > cfg.max_image_dimension || swatch_height > cfg.max_image_dimension {
        return Err(WmsError::InvalidParam {
            name: "width|height",
            reason: format!("max dimension is {}", cfg.max_image_dimension),
        });
    }

    let rule = kvp.get("rule").filter(|s| !s.is_empty()).cloned();

    Ok(LegendPlan {
        layer,
        format,
        swatch_width,
        swatch_height,
        rule,
    })
}

fn parse_optional_dim(kvp: &Kvp, name: &'static str) -> Result<Option<u32>, WmsError> {
    let raw = match kvp.get(name) {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(None),
    };
    let v: u32 = raw
        .parse()
        .map_err(|e: std::num::ParseIntError| WmsError::InvalidParam {
            name,
            reason: e.to_string(),
        })?;
    if v == 0 {
        return Err(WmsError::InvalidParam {
            name,
            reason: "must be > 0".into(),
        });
    }
    Ok(Some(v))
}
