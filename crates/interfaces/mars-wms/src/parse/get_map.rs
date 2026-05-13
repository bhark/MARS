//! `GetMap` KVP extraction. Produces an Option-heavy [`ParsedGetMap`]
//! consumed by [`crate::prepare::resolve_get_map`]; this layer only does
//! tokenisation and shape parsing (u32, f64) - all semantic validation
//! (allowlists, bounds, axis-aware bbox, defaults) lives in prepare.

use mars_types::LayerId;

use super::common::{nonempty, parse_kvp, parse_optional_u32, Kvp};
use crate::prepare::viewport::ParsedViewport;
use crate::prepare::{resolve_get_map, ParsedGetMap, ResolvedGetMap};
use crate::{WmsConfig, WmsError};

/// Parse a `GetMap` query-string and resolve it. Public facade used by the
/// dispatcher; tests; bins. Returns the runtime `RenderPlan` directly for
/// callers that don't care about EXCEPTIONS.
pub fn parse_get_map(query: &str, cfg: &WmsConfig) -> Result<mars_runtime::RenderPlan, WmsError> {
    let kvp = parse_kvp(query);
    let parsed = parse_get_map_kvp(&kvp)?;
    Ok(resolve_get_map(parsed, cfg)?.plan)
}

/// Parse + resolve in one step; used by the dispatcher when it needs both
/// the plan and EXCEPTIONS.
pub(super) fn resolve_get_map_from_kvp(kvp: &Kvp, cfg: &WmsConfig) -> Result<ResolvedGetMap, WmsError> {
    let parsed = parse_get_map_kvp(kvp)?;
    resolve_get_map(parsed, cfg)
}

/// KVP -> [`ParsedGetMap`]. Only fails on shape errors (e.g. `width=abc`
/// not a u32). Required-field and allowlist checks happen in prepare.
fn parse_get_map_kvp(kvp: &Kvp) -> Result<ParsedGetMap, WmsError> {
    Ok(ParsedGetMap {
        viewport: parse_viewport(kvp)?,
        exceptions: nonempty(kvp, "exceptions"),
    })
}

/// Shared viewport-KVP extractor used by GetMap (here) and reused by the
/// other ops once they migrate to prepare. Currently `pub(crate)` so the
/// per-op parsers can compose it.
pub(crate) fn parse_viewport(kvp: &Kvp) -> Result<ParsedViewport, WmsError> {
    Ok(ParsedViewport {
        version: nonempty(kvp, "version"),
        layers: parse_layers(kvp),
        crs: nonempty(kvp, "crs"),
        bbox: nonempty(kvp, "bbox"),
        width: parse_optional_u32(kvp, "width")?,
        height: parse_optional_u32(kvp, "height")?,
        format: nonempty(kvp, "format"),
        dpi: parse_optional_dpi(kvp)?,
    })
}

fn parse_layers(kvp: &Kvp) -> Option<Vec<LayerId>> {
    let raw = kvp.get("layers").filter(|s| !s.is_empty())?;
    Some(raw.split(',').filter(|s| !s.is_empty()).map(LayerId::new).collect())
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
    Ok(Some(dpi))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use mars_types::{CrsCode, ImageFormat};

    use super::super::parse_request;
    use super::*;
    use crate::ExceptionsFormat;

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
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:4326&\
                 bbox=10,20,11,22&width=1&height=1&format=image/png";
        let plan = parse_get_map(q, &cfg()).unwrap();
        assert_eq!(plan.bbox.min_x, 20.0);
        assert_eq!(plan.bbox.min_y, 10.0);
        assert_eq!(plan.bbox.max_x, 22.0);
        assert_eq!(plan.bbox.max_y, 11.0);
    }

    #[test]
    fn exceptions_default_is_xml() {
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png";
        let req = parse_request(q, &cfg()).unwrap();
        match req {
            crate::WmsRequest::GetMap(r) => assert_eq!(r.exceptions, ExceptionsFormat::Xml),
            _ => panic!("expected GetMap"),
        }
    }

    #[test]
    fn exceptions_blank_accepted() {
        for kw in ["BLANK", "blank", "application/vnd.ogc.se_blank"] {
            let q = format!(
                "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png&exceptions={kw}"
            );
            let req = parse_request(&q, &cfg()).unwrap();
            match req {
                crate::WmsRequest::GetMap(r) => {
                    assert_eq!(r.exceptions, ExceptionsFormat::Blank, "kw={kw}")
                }
                _ => panic!("expected GetMap"),
            }
        }
    }

    #[test]
    fn exceptions_xml_keyword_accepted() {
        for kw in ["XML", "xml", "application/vnd.ogc.se_xml"] {
            let q = format!(
                "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png&exceptions={kw}"
            );
            let req = parse_request(&q, &cfg()).unwrap();
            match req {
                crate::WmsRequest::GetMap(r) => {
                    assert_eq!(r.exceptions, ExceptionsFormat::Xml, "kw={kw}")
                }
                _ => panic!("expected GetMap"),
            }
        }
    }

    #[test]
    fn exceptions_inimage_rejected_as_not_implemented() {
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png&exceptions=INIMAGE";
        let err = parse_request(q, &cfg()).unwrap_err();
        assert!(matches!(err, WmsError::NotImplemented { .. }));
    }

    #[test]
    fn exceptions_unknown_rejected() {
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png&exceptions=GARBAGE";
        let err = parse_request(q, &cfg()).unwrap_err();
        assert!(matches!(err, WmsError::InvalidParam { name: "exceptions", .. }));
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
    fn empty_layer_name_filtered_out() {
        let q = "request=GetMap&version=1.3.0&layers=a,,b&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png";
        let plan = parse_get_map(q, &cfg()).unwrap();
        assert_eq!(plan.layers.len(), 2);
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
