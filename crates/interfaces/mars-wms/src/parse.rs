//! WMS 1.3.0 KVP request parsing.
//!
//! Covers `GetMap` and `GetCapabilities`. Other request kinds reject
//! with `WmsError::NotImplemented` so they round-trip to an XML exception in
//! the edge.
//!
//! KVP semantics: parameter names are case-insensitive (lowercased on parse,
//! per OGC 06-042 §11.5.2); values are preserved as-is. Repeated keys
//! follow last-win semantics — the spec does not pin a behaviour, so this
//! is an adapter choice that matches common WMS server practice.

use std::collections::HashMap;

use percent_encoding::percent_decode_str;

use mars_runtime::RenderPlan;
use mars_types::{Bbox, CrsCode, ImageFormat, LayerId};

use crate::{WmsConfig, WmsError, WmsRequest};

/// Parse any WMS request, dispatching on the `request=` parameter.
pub fn parse_request(query: &str, cfg: &WmsConfig) -> Result<WmsRequest, WmsError> {
    let kvp = parse_kvp(query);
    let request = require(&kvp, "request")?;
    match request.as_str() {
        s if s.eq_ignore_ascii_case("GetMap") => Ok(WmsRequest::GetMap(parse_get_map_inner(&kvp, cfg)?)),
        s if s.eq_ignore_ascii_case("GetCapabilities") => Ok(WmsRequest::GetCapabilities),
        other => Err(WmsError::NotImplemented {
            what: format!("WMS request={other}"),
        }),
    }
}

/// Parse a `GetMap` query-string into a [`RenderPlan`]. Also accepts the
/// `request=GetMap` parameter but does not require it; the dispatcher in
/// [`parse_request`] checks that.
pub fn parse_get_map(query: &str, cfg: &WmsConfig) -> Result<RenderPlan, WmsError> {
    let kvp = parse_kvp(query);
    parse_get_map_inner(&kvp, cfg)
}

fn parse_get_map_inner(kvp: &Kvp, cfg: &WmsConfig) -> Result<RenderPlan, WmsError> {
    // version is checked loosely; SPEC commits to 1.3.0 only.
    if let Some(v) = kvp.get("version")
        && v != "1.3.0"
    {
        return Err(WmsError::InvalidParam {
            name: "version",
            reason: format!("only 1.3.0 supported, got {v}"),
        });
    }

    let layers_raw = require(kvp, "layers")?;
    let layers: Vec<LayerId> = layers_raw
        .split(',')
        .filter(|s| !s.is_empty())
        .map(LayerId::new)
        .collect();
    if layers.is_empty() {
        return Err(WmsError::InvalidParam {
            name: "layers",
            reason: "no layer names".into(),
        });
    }
    if layers.len() > cfg.max_layers {
        return Err(WmsError::InvalidParam {
            name: "layers",
            reason: format!("{} exceeds max {}", layers.len(), cfg.max_layers),
        });
    }

    let crs_raw = require(kvp, "crs")?;
    let crs = CrsCode::new(crs_raw.as_str());
    if !cfg.allowlist_crs.is_empty() && !cfg.allowlist_crs.iter().any(|c| c.as_str() == crs_raw) {
        return Err(WmsError::InvalidParam {
            name: "crs",
            reason: format!("{crs_raw} not in reprojection allowlist"),
        });
    }

    let bbox_raw = require(kvp, "bbox")?;
    let bbox = parse_bbox(&bbox_raw, &crs_raw, cfg.max_bbox_coord)?;

    let width = parse_u32(kvp, "width")?;
    let height = parse_u32(kvp, "height")?;
    if width == 0 || height == 0 {
        return Err(WmsError::InvalidParam {
            name: "width|height",
            reason: "must be > 0".into(),
        });
    }
    if width > cfg.max_image_dimension || height > cfg.max_image_dimension {
        return Err(WmsError::InvalidParam {
            name: "width|height",
            reason: format!("max dimension is {}, got {}x{}", cfg.max_image_dimension, width, height),
        });
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > cfg.max_pixels {
        return Err(WmsError::InvalidParam {
            name: "width|height",
            reason: format!(
                "max pixels per request is {}, got {} ({}x{})",
                cfg.max_pixels, pixels, width, height
            ),
        });
    }

    let format_raw = require(kvp, "format")?;
    let format = parse_format(&format_raw)?;
    if !cfg.formats.is_empty() && !cfg.formats.contains(&format) {
        return Err(WmsError::InvalidParam {
            name: "format",
            reason: format!("{format_raw} not enabled"),
        });
    }

    // optional `&DPI=` (or `&MAP_RESOLUTION=`, MapServer's name) overrides
    // the service-default scale dpi for this one request. lets clients pin
    // their own display dpi when computing scale-window routing without
    // touching service config.
    let scale_pixel_size_m = match parse_optional_dpi(kvp)? {
        Some(dpi) => 0.0254 / dpi,
        None => cfg.scale_pixel_size_m,
    };

    Ok(RenderPlan {
        layers,
        bbox,
        width,
        height,
        crs,
        format,
        scale_pixel_size_m,
    })
}

fn parse_optional_dpi(kvp: &Kvp) -> Result<Option<f64>, WmsError> {
    let raw = match kvp.get("dpi").or_else(|| kvp.get("map_resolution")) {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(None),
    };
    let dpi: f64 = raw
        .parse()
        .map_err(|e: std::num::ParseFloatError| WmsError::InvalidParam {
            name: "dpi",
            reason: e.to_string(),
        })?;
    if !dpi.is_finite() || dpi <= 0.0 {
        return Err(WmsError::InvalidParam {
            name: "dpi",
            reason: "must be a positive, finite number".into(),
        });
    }
    Ok(Some(dpi))
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
        // wms is case-insensitive on parameter names per OGC 06-042
        out.insert(k.to_ascii_lowercase(), pct_decode(v));
    }
    out
}

/// percent-decode a KVP value with `+` -> space (form-style). invalid escapes
/// pass through literally, matching the prior hand-rolled behaviour.
fn pct_decode(s: &str) -> String {
    // form-style first: + means space.
    let plus_decoded: String = s.chars().map(|c| if c == '+' { ' ' } else { c }).collect();
    percent_decode_str(&plus_decoded).decode_utf8_lossy().into_owned()
}

fn require(kvp: &Kvp, name: &'static str) -> Result<String, WmsError> {
    kvp.get(name)
        .filter(|s| !s.is_empty())
        .cloned()
        .ok_or(WmsError::MissingParam(name))
}

fn parse_u32(kvp: &Kvp, name: &'static str) -> Result<u32, WmsError> {
    let raw = require(kvp, name)?;
    raw.parse()
        .map_err(|e: std::num::ParseIntError| WmsError::InvalidParam {
            name,
            reason: e.to_string(),
        })
}

fn parse_format(raw: &str) -> Result<ImageFormat, WmsError> {
    match raw {
        "image/png" => Ok(ImageFormat::Png),
        "image/jpeg" | "image/jpg" => Ok(ImageFormat::Jpeg),
        other => Err(WmsError::InvalidParam {
            name: "format",
            reason: format!("unsupported {other}"),
        }),
    }
}

/// WMS 1.3.0 axis-order rule: for CRSes with declared lat/lon (north/east) axis
/// order, BBOX is `miny,minx,maxy,maxx` (lat,lon ordering). For CRSes with
/// east/north axis order it is the natural `minx,miny,maxx,maxy`.
///
/// Ships a small CRS allowlist; EPSG:4326 is the canonical lat/lon case.
/// EPSG:25832 and EPSG:3857 are east/north. Adding more is a one-line edit;
/// upstream PROJ axis introspection lands with reprojection in a future release.
fn is_lat_lon_order(crs: &str) -> bool {
    matches!(crs, "EPSG:4326" | "urn:ogc:def:crs:EPSG::4326")
}

fn parse_bbox(raw: &str, crs: &str, max_coord: f64) -> Result<Bbox, WmsError> {
    let parts: Vec<&str> = raw.split(',').collect();
    if parts.len() != 4 {
        return Err(WmsError::InvalidParam {
            name: "bbox",
            reason: "expected 4 comma-separated floats".into(),
        });
    }
    let nums: Vec<f64> = parts
        .iter()
        .map(|s| s.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|e| WmsError::InvalidParam {
            name: "bbox",
            reason: e.to_string(),
        })?;
    let (min_x, min_y, max_x, max_y) = if is_lat_lon_order(crs) {
        // wire order: minLat,minLon,maxLat,maxLon -> internal (x=lon, y=lat)
        (nums[1], nums[0], nums[3], nums[2])
    } else {
        (nums[0], nums[1], nums[2], nums[3])
    };
    for v in [min_x, min_y, max_x, max_y] {
        if !v.is_finite() {
            return Err(WmsError::InvalidParam {
                name: "bbox",
                reason: "coordinates must be finite".into(),
            });
        }
        if v.abs() > max_coord {
            return Err(WmsError::InvalidParam {
                name: "bbox",
                reason: format!("coordinate magnitude exceeds {max_coord}"),
            });
        }
    }
    if !(max_x > min_x && max_y > min_y) {
        return Err(WmsError::InvalidParam {
            name: "bbox",
            reason: "max must exceed min on both axes".into(),
        });
    }
    Ok(Bbox::new(min_x, min_y, max_x, max_y))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn cfg() -> WmsConfig {
        WmsConfig {
            allowlist_crs: vec![CrsCode::new("EPSG:25832"), CrsCode::new("EPSG:4326")],
            formats: vec![ImageFormat::Png],
            max_image_dimension: 8192,
            max_pixels: 16_000_000,
            max_layers: 100,
            max_bbox_coord: 1e9,
            scale_pixel_size_m: 0.0254 / 96.0,
        }
    }

    #[test]
    fn happy_path() {
        let q = "service=WMS&version=1.3.0&request=GetMap&layers=a,b&styles=&\
                 crs=EPSG:25832&bbox=100,200,300,400&width=256&height=128&format=image/png";
        let plan = parse_get_map(q, &cfg()).unwrap();
        assert_eq!(plan.layers.len(), 2);
        assert_eq!(plan.width, 256);
        assert_eq!(plan.height, 128);
        assert_eq!(plan.bbox.min_x, 100.0);
        assert_eq!(plan.bbox.max_y, 400.0);
        assert_eq!(plan.crs.as_str(), "EPSG:25832");
        assert_eq!(plan.format, ImageFormat::Png);
        // no &DPI=, expect cfg default (96).
        assert!((plan.scale_pixel_size_m - 0.0254 / 96.0).abs() < 1e-12);
    }

    #[test]
    fn dpi_override_per_request() {
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png&dpi=72";
        let plan = parse_get_map(q, &cfg()).unwrap();
        assert!((plan.scale_pixel_size_m - 0.0254 / 72.0).abs() < 1e-12);
    }

    #[test]
    fn map_resolution_alias_accepted() {
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png&map_resolution=120";
        let plan = parse_get_map(q, &cfg()).unwrap();
        assert!((plan.scale_pixel_size_m - 0.0254 / 120.0).abs() < 1e-12);
    }

    #[test]
    fn dpi_invalid_rejected() {
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png&dpi=-5";
        let err = parse_get_map(q, &cfg()).unwrap_err();
        assert!(matches!(err, WmsError::InvalidParam { name: "dpi", .. }));
    }

    #[test]
    fn missing_param() {
        let q = "request=GetMap&version=1.3.0";
        let err = parse_get_map(q, &cfg()).unwrap_err();
        assert!(matches!(err, WmsError::MissingParam("layers")));
    }

    #[test]
    fn bad_bbox() {
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=oops&width=1&height=1&format=image/png";
        let err = parse_get_map(q, &cfg()).unwrap_err();
        assert!(matches!(err, WmsError::InvalidParam { name: "bbox", .. }));
    }

    #[test]
    fn axis_swap_4326() {
        // wire: minLat=10, minLon=20, maxLat=11, maxLon=22
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:4326&\
                 bbox=10,20,11,22&width=1&height=1&format=image/png";
        let plan = parse_get_map(q, &cfg()).unwrap();
        assert_eq!(plan.bbox.min_x, 20.0);
        assert_eq!(plan.bbox.min_y, 10.0);
        assert_eq!(plan.bbox.max_x, 22.0);
        assert_eq!(plan.bbox.max_y, 11.0);
    }

    #[test]
    fn dispatch_capabilities() {
        let q = "service=WMS&version=1.3.0&request=GetCapabilities";
        let req = parse_request(q, &cfg()).unwrap();
        assert!(matches!(req, WmsRequest::GetCapabilities));
    }

    #[test]
    fn percent_decode() {
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG%3A25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image%2Fpng";
        let plan = parse_get_map(q, &cfg()).unwrap();
        assert_eq!(plan.crs.as_str(), "EPSG:25832");
    }

    #[test]
    fn width_too_large() {
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=9000&height=1&format=image/png";
        let err = parse_get_map(q, &cfg()).unwrap_err();
        assert!(matches!(
            err,
            WmsError::InvalidParam {
                name: "width|height",
                ..
            }
        ));
    }

    #[test]
    fn too_many_layers() {
        let q = format!(
            "request=GetMap&version=1.3.0&layers={}&crs=EPSG:25832&\
             bbox=0,0,1,1&width=1&height=1&format=image/png",
            (0..101).map(|i| i.to_string()).collect::<Vec<_>>().join(",")
        );
        let err = parse_get_map(&q, &cfg()).unwrap_err();
        assert!(matches!(err, WmsError::InvalidParam { name: "layers", .. }));
    }

    #[test]
    fn bbox_non_finite() {
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,inf&width=1&height=1&format=image/png";
        let err = parse_get_map(q, &cfg()).unwrap_err();
        assert!(matches!(err, WmsError::InvalidParam { name: "bbox", .. }));
    }

    #[test]
    fn bbox_coord_too_large() {
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,1e10&width=1&height=1&format=image/png";
        let err = parse_get_map(q, &cfg()).unwrap_err();
        assert!(matches!(err, WmsError::InvalidParam { name: "bbox", .. }));
    }

    #[test]
    fn malformed_percent_encoding_passes_through() {
        // %ZZ and %G are invalid hex → passed through literally in the value
        let q = "request=GetMap&version=1.3.0&layers=foo%ZZ%G&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png";
        let plan = parse_get_map(q, &cfg()).unwrap();
        assert_eq!(plan.layers[0].as_str(), "foo%ZZ%G");
    }

    #[test]
    fn percent_decode_at_end_of_input() {
        // boundary: %xx at the very end of the value must decode (the previous
        // hand-rolled implementation had an off-by-one that emitted it literally).
        // `%2F` decodes to `/`; we feed it as the final 3 bytes of a value.
        let q = "request=GetMap&version=1.3.0&layers=ab%2F&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png";
        let plan = parse_get_map(q, &cfg()).unwrap();
        assert_eq!(plan.layers[0].as_str(), "ab/");
    }

    #[test]
    fn bbox_max_equals_min_rejected() {
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,0,1&width=1&height=1&format=image/png";
        let err = parse_get_map(q, &cfg()).unwrap_err();
        assert!(matches!(err, WmsError::InvalidParam { name: "bbox", .. }));
    }

    #[test]
    fn empty_layer_name_filtered_out() {
        let q = "request=GetMap&version=1.3.0&layers=a,,b&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png";
        let plan = parse_get_map(q, &cfg()).unwrap();
        assert_eq!(plan.layers.len(), 2);
    }

    #[test]
    fn multiple_equals_in_value() {
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png&custom=val=ue";
        let plan = parse_get_map(q, &cfg()).unwrap();
        assert_eq!(plan.layers.len(), 1);
    }

    #[test]
    fn unsupported_format_rejected() {
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/tiff";
        let err = parse_get_map(q, &cfg()).unwrap_err();
        assert!(matches!(err, WmsError::InvalidParam { name: "format", .. }));
    }

    #[test]
    fn width_at_u32_max_parseable() {
        let q = format!(
            "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
             bbox=0,0,1,1&width={}&height=1&format=image/png",
            u32::MAX
        );
        let err = parse_get_map(&q, &cfg()).unwrap_err();
        assert!(matches!(
            err,
            WmsError::InvalidParam {
                name: "width|height",
                ..
            }
        ));
    }

    #[test]
    fn crs_not_in_allowlist_rejected() {
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:3857&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png";
        let err = parse_get_map(q, &cfg()).unwrap_err();
        assert!(matches!(err, WmsError::InvalidParam { name: "crs", .. }));
    }
}
