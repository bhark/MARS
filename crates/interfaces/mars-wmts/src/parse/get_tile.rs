//! WMTS `GetTile` extraction for both the KVP transport (`/wmts?...`) and
//! the REST resource path
//! (`/wmts/{Layer}/{Style}/{TileMatrixSet}/{TileMatrix}/{TileRow}/{TileCol}.{ext}`).
//!
//! Both transports lower to a single [`ParsedGetTile`] shape; semantic
//! validation and bbox math live in [`crate::prepare::resolve_get_tile`].
//! That single chokepoint guarantees REST and KVP cache keys never drift.

use mars_runtime::RenderPlan;
use mars_types::ImageFormat;

use super::common::{Kvp, parse_kvp, parse_optional_u32};
use crate::prepare::{ParsedGetTile, ResolvedGetTile, resolve_get_tile};
use crate::{WmtsConfig, WmtsError};

/// Parse a KVP `GetTile` query-string into a [`RenderPlan`].
pub fn parse_get_tile(query: &str, cfg: &WmtsConfig) -> Result<RenderPlan, WmtsError> {
    let kvp = parse_kvp(query);
    Ok(resolve_get_tile_from_kvp(&kvp, cfg)?.plan)
}

pub(super) fn resolve_get_tile_from_kvp(kvp: &Kvp, cfg: &WmtsConfig) -> Result<ResolvedGetTile, WmtsError> {
    let parsed = parse_kvp_get_tile(kvp)?;
    resolve_get_tile(parsed, cfg)
}

fn parse_kvp_get_tile(kvp: &Kvp) -> Result<ParsedGetTile, WmtsError> {
    Ok(ParsedGetTile {
        version: nonempty(kvp, "version"),
        layer: nonempty(kvp, "layer"),
        format: nonempty(kvp, "format").map(|s| parse_format_mime(&s)).transpose()?,
        tilematrixset: nonempty(kvp, "tilematrixset"),
        tilematrix: nonempty(kvp, "tilematrix"),
        tilecol: parse_optional_u32(kvp, "tilecol")?,
        tilerow: parse_optional_u32(kvp, "tilerow")?,
    })
}

fn nonempty(kvp: &Kvp, name: &str) -> Option<String> {
    kvp.get(name).filter(|s| !s.is_empty()).cloned()
}

/// Parse a REST-form tile request. The router strips the path prefix and
/// hands `layer/style/tms/z/y/x` plus the file extension; `ext` is the suffix
/// after the final `.` (e.g. `png`, `jpg`, `jpeg`).
///
/// `version` cannot be carried in the REST path - WMTS 1.0.0 is implicit.
/// `style` per spec may be the literal `default` to mean "no style filter";
/// that distinction is collapsed to empty here.
#[allow(clippy::too_many_arguments)]
pub fn parse_rest_get_tile(
    layer: &str,
    style: &str,
    tms: &str,
    z: &str,
    y: &str,
    x: &str,
    ext: &str,
    cfg: &WmtsConfig,
) -> Result<RenderPlan, WmtsError> {
    let parsed = parse_rest(layer, style, tms, z, y, x, ext)?;
    Ok(resolve_get_tile(parsed, cfg)?.plan)
}

fn parse_rest(
    layer: &str,
    _style: &str,
    tms: &str,
    z: &str,
    y: &str,
    x: &str,
    ext: &str,
) -> Result<ParsedGetTile, WmtsError> {
    let format = parse_format_ext(ext)?;
    let tile_col: u32 = x
        .parse()
        .map_err(|e: std::num::ParseIntError| WmtsError::InvalidParam {
            name: "tilecol",
            reason: e.to_string(),
        })?;
    let tile_row: u32 = y
        .parse()
        .map_err(|e: std::num::ParseIntError| WmtsError::InvalidParam {
            name: "tilerow",
            reason: e.to_string(),
        })?;
    // `style` (KVP or REST) is intentionally discarded: the renderer does
    // not yet route per-style. Restore the field once there's a consumer.
    Ok(ParsedGetTile {
        version: None,
        layer: Some(layer.to_owned()),
        format: Some(format),
        tilematrixset: Some(tms.to_owned()),
        tilematrix: Some(z.to_owned()),
        tilecol: Some(tile_col),
        tilerow: Some(tile_row),
    })
}

fn parse_format_mime(raw: &str) -> Result<ImageFormat, WmtsError> {
    match raw {
        "image/png" => Ok(ImageFormat::Png),
        "image/jpeg" | "image/jpg" => Ok(ImageFormat::Jpeg),
        other => Err(WmtsError::InvalidParam {
            name: "format",
            reason: format!("unsupported {other}"),
        }),
    }
}

/// Map a REST URL file extension to an [`ImageFormat`].
fn parse_format_ext(ext: &str) -> Result<ImageFormat, WmtsError> {
    match ext.to_ascii_lowercase().as_str() {
        "png" => Ok(ImageFormat::Png),
        "jpg" | "jpeg" => Ok(ImageFormat::Jpeg),
        other => Err(WmtsError::InvalidParam {
            name: "format",
            reason: format!("unsupported extension `.{other}`"),
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::BTreeMap;

    use mars_config::{TileMatrixLevel, TileMatrixSet};
    use mars_types::CrsCode;

    use super::*;

    const TEST_PIXEL_SIZE_M: f64 = 0.000_28;
    const METERS_PER_DEGREE_EQUATOR: f64 = 111_319.490_793_273_57;

    fn dk_tms() -> TileMatrixSet {
        TileMatrixSet {
            crs: CrsCode::new("EPSG:25832"),
            top_left: [120_000.0, 6_500_000.0],
            tile_size: [256, 256],
            levels: vec![
                TileMatrixLevel {
                    id: 0,
                    scale_denominator: 1638.4 / TEST_PIXEL_SIZE_M,
                    matrix_width: 1,
                    matrix_height: 1,
                },
                TileMatrixLevel {
                    id: 1,
                    scale_denominator: 819.2 / TEST_PIXEL_SIZE_M,
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
        let mut sets = BTreeMap::new();
        sets.insert(
            "world_4326".to_owned(),
            TileMatrixSet {
                crs: CrsCode::new("EPSG:4326"),
                top_left: [-180.0, 90.0],
                tile_size: [256, 256],
                levels: vec![TileMatrixLevel {
                    id: 0,
                    scale_denominator: METERS_PER_DEGREE_EQUATOR / TEST_PIXEL_SIZE_M,
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
    fn rest_get_tile_matches_kvp_plan() {
        // load-bearing: REST and KVP must produce byte-identical RenderPlans
        // for the same tile so cache-key parity holds.
        let kvp = parse_get_tile(
            "request=GetTile&layer=roads&style=&format=image/png&\
             tilematrixset=dk_25832&tilematrix=0&tilecol=2&tilerow=3",
            &cfg(),
        )
        .unwrap();
        let rest = parse_rest_get_tile("roads", "default", "dk_25832", "0", "3", "2", "png", &cfg()).unwrap();
        assert_eq!(kvp.layers, rest.layers);
        assert_eq!(kvp.width, rest.width);
        assert_eq!(kvp.height, rest.height);
        assert_eq!(kvp.format, rest.format);
        assert_eq!(kvp.crs.as_str(), rest.crs.as_str());
        assert!((kvp.bbox.min_x - rest.bbox.min_x).abs() < 1e-9);
        assert!((kvp.bbox.max_y - rest.bbox.max_y).abs() < 1e-9);
        assert!((kvp.bbox.max_x - rest.bbox.max_x).abs() < 1e-9);
        assert!((kvp.bbox.min_y - rest.bbox.min_y).abs() < 1e-9);
    }

    #[test]
    fn rest_jpeg_extension_accepted() {
        let mut c = cfg();
        c.formats.push(ImageFormat::Jpeg);
        let plan = parse_rest_get_tile("a", "default", "dk_25832", "0", "0", "0", "jpg", &c).unwrap();
        assert_eq!(plan.format, ImageFormat::Jpeg);
        let plan = parse_rest_get_tile("a", "default", "dk_25832", "0", "0", "0", "JPEG", &c).unwrap();
        assert_eq!(plan.format, ImageFormat::Jpeg);
    }

    #[test]
    fn rest_empty_style_equivalent_to_default() {
        let empty = parse_rest_get_tile("a", "", "dk_25832", "0", "0", "0", "png", &cfg()).unwrap();
        let default = parse_rest_get_tile("a", "default", "dk_25832", "0", "0", "0", "png", &cfg()).unwrap();
        assert_eq!(empty.layers, default.layers);
        assert_eq!(empty.bbox.min_x, default.bbox.min_x);
    }

    #[test]
    fn rest_unknown_extension_rejected() {
        let err = parse_rest_get_tile("a", "default", "dk_25832", "0", "0", "0", "tiff", &cfg()).unwrap_err();
        assert!(matches!(err, WmtsError::InvalidParam { name: "format", .. }));
    }

    #[test]
    fn bbox_clamp_rejects_runaway_col() {
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
