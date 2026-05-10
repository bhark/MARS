//! WMTS 1.0.0 KVP request parsing.
//!
//! v1 covers `GetTile` and `GetCapabilities`. Other request kinds
//! (`GetFeatureInfo`) reject with `WmtsError::NotImplemented`.

use std::collections::HashMap;

use percent_encoding::percent_decode_str;

use mars_config::TileMatrixSet;
use mars_runtime::RenderPlan;
use mars_types::{Bbox, ImageFormat, LayerId};

use crate::{WmtsConfig, WmtsError, WmtsRequest};

/// Parse any WMTS request, dispatching on the `request=` parameter.
pub fn parse_request(query: &str, cfg: &WmtsConfig) -> Result<WmtsRequest, WmtsError> {
    let kvp = parse_kvp(query);
    let request = require(&kvp, "request")?;
    match request.as_str() {
        s if s.eq_ignore_ascii_case("GetTile") => Ok(WmtsRequest::GetTile(parse_get_tile_inner(&kvp, cfg)?)),
        s if s.eq_ignore_ascii_case("GetCapabilities") => Ok(WmtsRequest::GetCapabilities),
        other => Err(WmtsError::NotImplemented {
            what: format!("WMTS request={other}"),
        }),
    }
}

/// Parse a `GetTile` query-string into a [`RenderPlan`].
pub fn parse_get_tile(query: &str, cfg: &WmtsConfig) -> Result<RenderPlan, WmtsError> {
    let kvp = parse_kvp(query);
    parse_get_tile_inner(&kvp, cfg)
}

fn parse_get_tile_inner(kvp: &Kvp, cfg: &WmtsConfig) -> Result<RenderPlan, WmtsError> {
    if let Some(v) = kvp.get("version")
        && v != "1.0.0"
    {
        return Err(WmtsError::InvalidParam {
            name: "version",
            reason: format!("only 1.0.0 supported, got {v}"),
        });
    }

    // wmts is single-layer per request (one tile, one layer); style is similarly single.
    let layer_raw = require(kvp, "layer")?;
    let layer = LayerId::new(layer_raw);

    // STYLE is required by spec but may be empty (default style); keep loose.
    let _style = kvp.get("style").cloned().unwrap_or_default();

    let format_raw = require(kvp, "format")?;
    let format = parse_format(&format_raw)?;
    if !cfg.formats.is_empty() && !cfg.formats.contains(&format) {
        return Err(WmtsError::InvalidParam {
            name: "format",
            reason: format!("{format_raw} not enabled"),
        });
    }

    let tms_name = require(kvp, "tilematrixset")?;
    let tms = cfg
        .tile_matrix_sets
        .get(&tms_name)
        .ok_or_else(|| WmtsError::InvalidParam {
            name: "tilematrixset",
            reason: format!("unknown tile matrix set `{tms_name}`"),
        })?;

    // tilematrix is the level identifier. The OGC spec models it as a string
    // (matrix identifier); MARS config uses numeric `id`. Accept both forms:
    // a bare integer matches `level.id`, anything else must equal a future
    // string identifier (none today).
    let tm_raw = require(kvp, "tilematrix")?;
    let level_id: u32 = tm_raw.parse().map_err(|_| WmtsError::InvalidParam {
        name: "tilematrix",
        reason: format!("expected integer level id, got `{tm_raw}`"),
    })?;
    let level = tms
        .levels
        .iter()
        .find(|l| l.id == level_id)
        .ok_or_else(|| WmtsError::InvalidParam {
            name: "tilematrix",
            reason: format!("level {level_id} not declared in `{tms_name}`"),
        })?;

    let tile_col = parse_u32(kvp, "tilecol")?;
    let tile_row = parse_u32(kvp, "tilerow")?;

    let bbox = tile_bbox(tms, level.scale_denominator, tile_col, tile_row, cfg.max_bbox_coord)?;

    let [w, h] = tms.tile_size;
    if w == 0 || h == 0 {
        return Err(WmtsError::InvalidParam {
            name: "tilematrixset",
            reason: format!("`{tms_name}` declares zero tile_size"),
        });
    }

    Ok(RenderPlan {
        layers: vec![layer],
        bbox,
        width: w,
        height: h,
        crs: tms.crs.clone(),
        format,
        // WMTS scale denominators are spec-fixed at the OGC standardised
        // pixel size; honouring service.scale_dpi here would desync routing
        // from the TileMatrixSet definition.
        scale_pixel_size_m: mars_runtime::OGC_STANDARDIZED_PIXEL_SIZE_M,
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
        // projected metric CRSes
        "EPSG:25832" | "EPSG:25833" | "EPSG:3857" | "EPSG:3034" | "EPSG:3035" => Some(1.0),
        // geographic, degrees
        "EPSG:4326" | "urn:ogc:def:crs:EPSG::4326" | "CRS84" | "urn:ogc:def:crs:OGC:1.3:CRS84" => {
            Some(METERS_PER_DEGREE_EQUATOR)
        }
        _ => None,
    }
}

// ---------- helpers ----------

type Kvp = HashMap<String, String>;

fn parse_kvp(query: &str) -> Kvp {
    let mut out = HashMap::new();
    for pair in query.trim_start_matches('?').split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(k.to_ascii_lowercase(), pct_decode(v));
    }
    out
}

fn pct_decode(s: &str) -> String {
    let plus_decoded: String = s.chars().map(|c| if c == '+' { ' ' } else { c }).collect();
    percent_decode_str(&plus_decoded).decode_utf8_lossy().into_owned()
}

fn require(kvp: &Kvp, name: &'static str) -> Result<String, WmtsError> {
    kvp.get(name)
        .filter(|s| !s.is_empty())
        .cloned()
        .ok_or(WmtsError::MissingParam(name))
}

fn parse_u32(kvp: &Kvp, name: &'static str) -> Result<u32, WmtsError> {
    let raw = require(kvp, name)?;
    raw.parse()
        .map_err(|e: std::num::ParseIntError| WmtsError::InvalidParam {
            name,
            reason: e.to_string(),
        })
}

fn parse_format(raw: &str) -> Result<ImageFormat, WmtsError> {
    match raw {
        "image/png" => Ok(ImageFormat::Png),
        "image/jpeg" | "image/jpg" => Ok(ImageFormat::Jpeg),
        other => Err(WmtsError::InvalidParam {
            name: "format",
            reason: format!("unsupported {other}"),
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::BTreeMap;

    use mars_config::TileMatrixLevel;
    use mars_types::CrsCode;

    use super::*;

    fn dk_tms() -> TileMatrixSet {
        // mimics a minimal `dk_25832`-shaped matrix set:
        // metric CRS, top-left at the typical extent corner, 256-px tiles.
        TileMatrixSet {
            crs: CrsCode::new("EPSG:25832"),
            top_left: [120_000.0, 6_500_000.0],
            tile_size: [256, 256],
            levels: vec![
                TileMatrixLevel {
                    id: 0,
                    // 1638.4 m/px at level 0 with 256-px tiles = 419430.4 m per tile.
                    scale_denominator: 1638.4 / STANDARDIZED_PIXEL_SIZE_M,
                    matrix_width: 1,
                    matrix_height: 1,
                },
                TileMatrixLevel {
                    id: 1,
                    scale_denominator: 819.2 / STANDARDIZED_PIXEL_SIZE_M,
                    matrix_width: 2,
                    matrix_height: 2,
                },
            ],
        }
    }

    fn cfg() -> WmtsConfig {
        let mut sets = BTreeMap::new();
        sets.insert("dk_25832".to_owned(), dk_tms());
        WmtsConfig {
            tile_matrix_sets: sets,
            formats: vec![ImageFormat::Png],
            max_bbox_coord: 1e9,
        }
    }

    #[test]
    fn happy_path() {
        let q = "service=WMTS&version=1.0.0&request=GetTile&layer=roads&style=&\
                 format=image/png&tilematrixset=dk_25832&tilematrix=0&tilecol=0&tilerow=0";
        let plan = parse_get_tile(q, &cfg()).unwrap();
        assert_eq!(plan.layers.len(), 1);
        assert_eq!(plan.layers[0].as_str(), "roads");
        assert_eq!(plan.crs.as_str(), "EPSG:25832");
        assert_eq!(plan.width, 256);
        assert_eq!(plan.height, 256);
        // tile (0,0) at level 0: top-left corner is the TMS top-left, span is
        // pixel_size_units * tile_size. pixel_size_units = 1638.4 m at L0.
        let expected_span = 1638.4 * 256.0;
        assert!((plan.bbox.min_x - 120_000.0).abs() < 1e-6);
        assert!((plan.bbox.max_x - (120_000.0 + expected_span)).abs() < 1e-6);
        assert!((plan.bbox.max_y - 6_500_000.0).abs() < 1e-6);
        assert!((plan.bbox.min_y - (6_500_000.0 - expected_span)).abs() < 1e-6);
    }

    #[test]
    fn level_one_halves_span() {
        let q = "request=GetTile&layer=a&format=image/png&tilematrixset=dk_25832&\
                 tilematrix=1&tilecol=0&tilerow=0";
        let plan = parse_get_tile(q, &cfg()).unwrap();
        let expected_span = 819.2 * 256.0;
        assert!((plan.bbox.max_x - (120_000.0 + expected_span)).abs() < 1e-6);
    }

    #[test]
    fn col_row_offsets() {
        let q = "request=GetTile&layer=a&format=image/png&tilematrixset=dk_25832&\
                 tilematrix=0&tilecol=2&tilerow=3";
        let plan = parse_get_tile(q, &cfg()).unwrap();
        let span = 1638.4 * 256.0;
        assert!((plan.bbox.min_x - (120_000.0 + 2.0 * span)).abs() < 1e-6);
        assert!((plan.bbox.max_y - (6_500_000.0 - 3.0 * span)).abs() < 1e-6);
    }

    #[test]
    fn missing_layer() {
        let q = "request=GetTile&format=image/png&tilematrixset=dk_25832&\
                 tilematrix=0&tilecol=0&tilerow=0";
        let err = parse_get_tile(q, &cfg()).unwrap_err();
        assert!(matches!(err, WmtsError::MissingParam("layer")));
    }

    #[test]
    fn unknown_tms() {
        let q = "request=GetTile&layer=a&format=image/png&tilematrixset=nope&\
                 tilematrix=0&tilecol=0&tilerow=0";
        let err = parse_get_tile(q, &cfg()).unwrap_err();
        assert!(matches!(
            err,
            WmtsError::InvalidParam {
                name: "tilematrixset",
                ..
            }
        ));
    }

    #[test]
    fn unknown_level() {
        let q = "request=GetTile&layer=a&format=image/png&tilematrixset=dk_25832&\
                 tilematrix=99&tilecol=0&tilerow=0";
        let err = parse_get_tile(q, &cfg()).unwrap_err();
        assert!(matches!(err, WmtsError::InvalidParam { name: "tilematrix", .. }));
    }

    #[test]
    fn non_integer_level() {
        let q = "request=GetTile&layer=a&format=image/png&tilematrixset=dk_25832&\
                 tilematrix=foo&tilecol=0&tilerow=0";
        let err = parse_get_tile(q, &cfg()).unwrap_err();
        assert!(matches!(err, WmtsError::InvalidParam { name: "tilematrix", .. }));
    }

    #[test]
    fn unsupported_format_rejected() {
        let q = "request=GetTile&layer=a&format=image/tiff&tilematrixset=dk_25832&\
                 tilematrix=0&tilecol=0&tilerow=0";
        let err = parse_get_tile(q, &cfg()).unwrap_err();
        assert!(matches!(err, WmtsError::InvalidParam { name: "format", .. }));
    }

    #[test]
    fn dispatch_capabilities() {
        let q = "service=WMTS&version=1.0.0&request=GetCapabilities";
        let req = parse_request(q, &cfg()).unwrap();
        assert!(matches!(req, WmtsRequest::GetCapabilities));
    }

    #[test]
    fn unknown_request_not_implemented() {
        let q = "request=GetFeatureInfo&layer=a&format=image/png&tilematrixset=dk_25832&\
                 tilematrix=0&tilecol=0&tilerow=0&i=1&j=1";
        let err = parse_request(q, &cfg()).unwrap_err();
        assert!(matches!(err, WmtsError::NotImplemented { .. }));
    }

    #[test]
    fn version_mismatch_rejected() {
        let q = "request=GetTile&version=1.1.0&layer=a&format=image/png&tilematrixset=dk_25832&\
                 tilematrix=0&tilecol=0&tilerow=0";
        let err = parse_get_tile(q, &cfg()).unwrap_err();
        assert!(matches!(err, WmtsError::InvalidParam { name: "version", .. }));
    }

    #[test]
    fn percent_decode_works() {
        let q = "request=GetTile&layer=a&format=image%2Fpng&tilematrixset=dk_25832&\
                 tilematrix=0&tilecol=0&tilerow=0";
        let plan = parse_get_tile(q, &cfg()).unwrap();
        assert_eq!(plan.format, ImageFormat::Png);
    }

    #[test]
    fn geographic_crs_uses_meters_per_degree() {
        // a degree-based TMS sanity: at scale_denominator chosen so that
        // pixel_size_units = 1.0 degree, 256-tile spans 256 degrees.
        let mut sets = BTreeMap::new();
        sets.insert(
            "world_4326".to_owned(),
            TileMatrixSet {
                crs: CrsCode::new("EPSG:4326"),
                top_left: [-180.0, 90.0],
                tile_size: [256, 256],
                levels: vec![TileMatrixLevel {
                    id: 0,
                    // pixel_size_units = sd * 0.00028 / METERS_PER_DEGREE_EQUATOR
                    // pick sd so units=1.0
                    scale_denominator: METERS_PER_DEGREE_EQUATOR / STANDARDIZED_PIXEL_SIZE_M,
                    matrix_width: 2,
                    matrix_height: 1,
                }],
            },
        );
        let cfg = WmtsConfig {
            tile_matrix_sets: sets,
            formats: vec![ImageFormat::Png],
            max_bbox_coord: 1e9,
        };
        let q = "request=GetTile&layer=a&format=image/png&tilematrixset=world_4326&\
                 tilematrix=0&tilecol=0&tilerow=0";
        let plan = parse_get_tile(q, &cfg).unwrap();
        assert!((plan.bbox.min_x - -180.0).abs() < 1e-9);
        assert!((plan.bbox.max_x - 76.0).abs() < 1e-9);
        assert!((plan.bbox.max_y - 90.0).abs() < 1e-9);
        assert!((plan.bbox.min_y - -166.0).abs() < 1e-9);
    }

    #[test]
    fn unknown_crs_meters_per_unit_rejected() {
        let mut sets = BTreeMap::new();
        sets.insert(
            "weird".to_owned(),
            TileMatrixSet {
                crs: CrsCode::new("EPSG:99999"),
                top_left: [0.0, 0.0],
                tile_size: [256, 256],
                levels: vec![TileMatrixLevel {
                    id: 0,
                    scale_denominator: 1000.0,
                    matrix_width: 1,
                    matrix_height: 1,
                }],
            },
        );
        let cfg = WmtsConfig {
            tile_matrix_sets: sets,
            formats: vec![ImageFormat::Png],
            max_bbox_coord: 1e9,
        };
        let q = "request=GetTile&layer=a&format=image/png&tilematrixset=weird&\
                 tilematrix=0&tilecol=0&tilerow=0";
        let err = parse_get_tile(q, &cfg).unwrap_err();
        assert!(matches!(
            err,
            WmtsError::InvalidParam {
                name: "tilematrixset",
                ..
            }
        ));
    }

    #[test]
    fn bbox_clamp_rejects_runaway_col() {
        // tilecol large enough to push past max_bbox_coord (1e9)
        let q = "request=GetTile&layer=a&format=image/png&tilematrixset=dk_25832&\
                 tilematrix=0&tilecol=10000000&tilerow=0";
        let err = parse_get_tile(q, &cfg()).unwrap_err();
        assert!(matches!(
            err,
            WmtsError::InvalidParam {
                name: "tilerow|tilecol",
                ..
            }
        ));
    }
}
