//! `GetMap` KVP parsing plus the `EXCEPTIONS=` and `DPI=` extensions that
//! only apply to GetMap.

use mars_runtime::RenderPlan;
use mars_types::{CrsCode, LayerId};

use super::common::{Kvp, parse_bbox, parse_format, parse_kvp, parse_u32, require};
use crate::{ExceptionsFormat, WmsConfig, WmsError};

/// Parse a `GetMap` query-string into a [`RenderPlan`]. Also accepts the
/// `request=GetMap` parameter but does not require it; the dispatcher in
/// [`super::parse_request`] checks that.
pub fn parse_get_map(query: &str, cfg: &WmsConfig) -> Result<RenderPlan, WmsError> {
    let kvp = parse_kvp(query);
    parse_get_map_inner(&kvp, cfg)
}

pub(super) fn parse_get_map_inner(kvp: &Kvp, cfg: &WmsConfig) -> Result<RenderPlan, WmsError> {
    // version is checked loosely; commits to 1.3.0 only.
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

/// Parse `&EXCEPTIONS=` per OGC 1.3.0. Optional; defaults to XML when absent.
/// Accepts the bare keyword forms most clients send (`XML`, `BLANK`,
/// `application/vnd.ogc.se_xml`, `application/vnd.ogc.se_blank`). INIMAGE is
/// recognised but rejected as `NotImplemented` so the wire error stays
/// faithful to spec instead of silently coercing.
pub(super) fn parse_exceptions(kvp: &Kvp) -> Result<ExceptionsFormat, WmsError> {
    let raw = match kvp.get("exceptions") {
        Some(s) if !s.is_empty() => s.as_str(),
        _ => return Ok(ExceptionsFormat::Xml),
    };
    if raw.eq_ignore_ascii_case("XML") || raw.eq_ignore_ascii_case("application/vnd.ogc.se_xml") {
        Ok(ExceptionsFormat::Xml)
    } else if raw.eq_ignore_ascii_case("BLANK") || raw.eq_ignore_ascii_case("application/vnd.ogc.se_blank") {
        Ok(ExceptionsFormat::Blank)
    } else if raw.eq_ignore_ascii_case("INIMAGE") || raw.eq_ignore_ascii_case("application/vnd.ogc.se_inimage") {
        Err(WmsError::NotImplemented {
            what: "EXCEPTIONS=INIMAGE".into(),
        })
    } else {
        Err(WmsError::InvalidParam {
            name: "exceptions",
            reason: format!("unsupported value `{raw}`"),
        })
    }
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use mars_types::ImageFormat;

    use super::super::parse_request;
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
    fn exceptions_default_is_xml() {
        let q = "request=GetMap&version=1.3.0&layers=a&crs=EPSG:25832&\
                 bbox=0,0,1,1&width=1&height=1&format=image/png";
        let req = parse_request(q, &cfg()).unwrap();
        match req {
            crate::WmsRequest::GetMap { exceptions, .. } => assert_eq!(exceptions, ExceptionsFormat::Xml),
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
                crate::WmsRequest::GetMap { exceptions, .. } => {
                    assert_eq!(exceptions, ExceptionsFormat::Blank, "kw={kw}")
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
                crate::WmsRequest::GetMap { exceptions, .. } => {
                    assert_eq!(exceptions, ExceptionsFormat::Xml, "kw={kw}")
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
