//! GetTile normalisation: takes a [`super::ParsedGetTile`] and produces a
//! validated [`ResolvedGetTile`]. KVP and REST both lower to `ParsedGetTile`
//! at the parse layer, then flow through this single resolver - so REST and
//! KVP cache keys / bbox math can never drift.

use mars_config::TileMatrixSet;
use mars_runtime::RenderPlan;
use mars_types::{Bbox, LayerId};

use super::ParsedGetTile;
use crate::{WmtsConfig, WmtsError};

/// Fully-validated GetTile request.
#[derive(Debug, Clone)]
pub struct ResolvedGetTile {
    pub plan: RenderPlan,
}

pub(crate) fn resolve_get_tile(p: ParsedGetTile, cfg: &WmtsConfig) -> Result<ResolvedGetTile, WmtsError> {
    if let Some(v) = &p.version
        && v != "1.0.0"
    {
        return Err(WmtsError::InvalidParam {
            name: "version",
            reason: format!("only 1.0.0 supported, got {v}"),
        });
    }

    let layer_raw = p.layer.as_deref().ok_or(WmtsError::MissingParam("layer"))?;
    if layer_raw.is_empty() {
        return Err(WmtsError::MissingParam("layer"));
    }
    let layer = LayerId::new(layer_raw.to_owned());

    let format = p.format.ok_or(WmtsError::MissingParam("format"))?;
    if !cfg.formats.is_empty() && !cfg.formats.contains(&format) {
        return Err(WmtsError::InvalidParam {
            name: "format",
            reason: format!("{} not enabled", format.mime()),
        });
    }

    let tms_name = p.tilematrixset.as_deref().ok_or(WmtsError::MissingParam("tilematrixset"))?;
    let tms = cfg
        .tile_matrix_sets
        .get(tms_name)
        .ok_or_else(|| WmtsError::InvalidParam {
            name: "tilematrixset",
            reason: format!("unknown tile matrix set `{tms_name}`"),
        })?;

    let tm_raw = p.tilematrix.as_deref().ok_or(WmtsError::MissingParam("tilematrix"))?;
    // tilematrix is the level identifier. The OGC spec models it as a string
    // (matrix identifier); MARS config uses numeric `id`. Accept the bare
    // integer form; string identifiers can land here when they're supported.
    let level_id: u32 = tm_raw.parse().map_err(|_| WmtsError::InvalidParam {
        name: "tilematrix",
        reason: format!("expected integer level id, got `{tm_raw}`"),
    })?;
    let level = tms.levels.iter().find(|l| l.id == level_id).ok_or_else(|| WmtsError::InvalidParam {
        name: "tilematrix",
        reason: format!("level {level_id} not declared in `{tms_name}`"),
    })?;

    let tile_col = p.tilecol.ok_or(WmtsError::MissingParam("tilecol"))?;
    let tile_row = p.tilerow.ok_or(WmtsError::MissingParam("tilerow"))?;

    let bbox = tile_bbox(tms, level.scale_denominator, tile_col, tile_row, cfg.max_bbox_coord)?;

    let [w, h] = tms.tile_size;
    if w == 0 || h == 0 {
        return Err(WmtsError::InvalidParam {
            name: "tilematrixset",
            reason: format!("`{tms_name}` declares zero tile_size"),
        });
    }

    Ok(ResolvedGetTile {
        plan: RenderPlan {
            layers: vec![layer],
            bbox,
            width: w,
            height: h,
            crs: tms.crs.clone(),
            format,
            // WMTS scale denominators are spec-fixed at the OGC standardised
            // pixel size; honouring service.scale_dpi would desync routing
            // from the TileMatrixSet definition.
            scale_pixel_size_m: mars_runtime::OGC_STANDARDIZED_PIXEL_SIZE_M,
        },
    })
}

/// Compute the bbox for `(tile_col, tile_row)` at the given scale denominator.
///
/// OGC WMTS standardised pixel size is 0.28 mm. `pixel_size_in_meters =
/// scale_denominator * 0.00028`. Conversion to CRS units divides by the CRS's
/// meters-per-unit; for projected metric CRSes that is 1.0, for geographic
/// CRSes (EPSG:4326) it is the equator-meter-per-degree constant.
fn tile_bbox(
    tms: &TileMatrixSet,
    scale_denominator: f64,
    tile_col: u32,
    tile_row: u32,
    max_coord: f64,
) -> Result<Bbox, WmtsError> {
    if !scale_denominator.is_finite() || scale_denominator <= 0.0 {
        return Err(WmtsError::InvalidParam {
            name: "tilematrix",
            reason: "scale_denominator must be positive and finite".into(),
        });
    }

    let meters_per_unit = meters_per_unit_for(tms.crs.as_str()).ok_or_else(|| WmtsError::InvalidParam {
        name: "tilematrixset",
        reason: format!("no meters-per-unit known for crs `{}`", tms.crs.as_str()),
    })?;

    let pixel_size_units = scale_denominator * STANDARDIZED_PIXEL_SIZE_M / meters_per_unit;
    let [tw, th] = tms.tile_size;
    let span_x = pixel_size_units * f64::from(tw);
    let span_y = pixel_size_units * f64::from(th);

    let min_x = tms.top_left[0] + f64::from(tile_col) * span_x;
    let max_x = min_x + span_x;
    let max_y = tms.top_left[1] - f64::from(tile_row) * span_y;
    let min_y = max_y - span_y;

    for v in [min_x, min_y, max_x, max_y] {
        if !v.is_finite() {
            return Err(WmtsError::InvalidParam {
                name: "tilerow|tilecol",
                reason: "computed bbox coordinate is non-finite".into(),
            });
        }
        if v.abs() > max_coord {
            return Err(WmtsError::InvalidParam {
                name: "tilerow|tilecol",
                reason: format!("computed bbox coordinate magnitude exceeds {max_coord}"),
            });
        }
    }

    Ok(Bbox::new(min_x, min_y, max_x, max_y))
}

/// OGC WMTS standardised pixel size (0.28 mm), used to derive ground span
/// from a level's scale denominator.
const STANDARDIZED_PIXEL_SIZE_M: f64 = 0.000_28;

/// Equator-meters per degree on a sphere of WGS84 mean radius. Standard
/// value baked into OGC well-known WGS84 TMS scale-set definitions.
const METERS_PER_DEGREE_EQUATOR: f64 = 111_319.490_793_273_57;

/// meters-per-unit lookup for the small CRS allowlist v1 ships with. Returns
/// `None` for unknown CRSes; operators wanting another CRS must add it here
/// (or, eventually, lift this into a PROJ-aware lookup).
fn meters_per_unit_for(crs: &str) -> Option<f64> {
    match crs {
        "EPSG:25832" | "EPSG:25833" | "EPSG:3857" | "EPSG:3034" | "EPSG:3035" => Some(1.0),
        "EPSG:4326" | "urn:ogc:def:crs:EPSG::4326" | "CRS84" | "urn:ogc:def:crs:OGC:1.3:CRS84" => {
            Some(METERS_PER_DEGREE_EQUATOR)
        }
        _ => None,
    }
}
