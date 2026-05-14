//! GetMap normalisation: takes a [`super::ParsedGetMap`] and produces a
//! validated [`ResolvedGetMap`]. Composes [`super::viewport::resolve_viewport`]
//! and folds the EXCEPTIONS= default in one place.

use mars_runtime::RenderPlan;

use super::ParsedGetMap;
use super::viewport::resolve_viewport;
use crate::{ExceptionsFormat, WmsConfig, WmsError, WmsVersion};

/// Fully-validated GetMap request. The dispatcher in
/// [`crate::parse::parse_request`] hands this to the handler, which reads
/// `plan` for rendering and `exceptions` for error-format selection.
#[derive(Debug, Clone)]
pub struct ResolvedGetMap {
    pub plan: RenderPlan,
    pub exceptions: ExceptionsFormat,
}

pub(crate) fn resolve_get_map(
    p: ParsedGetMap,
    cfg: &WmsConfig,
    version: WmsVersion,
) -> Result<ResolvedGetMap, WmsError> {
    let plan = resolve_viewport(&p.viewport, cfg, version)?;
    let exceptions = resolve_exceptions(p.exceptions.as_deref())?;
    Ok(ResolvedGetMap { plan, exceptions })
}

/// `&EXCEPTIONS=` per OGC 1.3.0. Optional; defaults to XML when absent.
/// Accepts the bare keyword forms most clients send (`XML`, `BLANK`,
/// `application/vnd.ogc.se_xml`, `application/vnd.ogc.se_blank`). `INIMAGE`
/// is recognised but rejected as `NotImplemented` so the wire error stays
/// faithful to spec instead of silently coercing.
fn resolve_exceptions(raw: Option<&str>) -> Result<ExceptionsFormat, WmsError> {
    let raw = match raw {
        Some(s) if !s.is_empty() => s,
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use mars_types::{CrsCode, ImageFormat, LayerId};

    use super::super::{ParsedGetMap, viewport::ParsedViewport};
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

    fn happy_viewport() -> ParsedViewport {
        ParsedViewport {
            layers: Some(vec![LayerId::new("a")]),
            crs: Some("EPSG:25832".into()),
            bbox: Some("0,0,1,1".into()),
            width: Some(1),
            height: Some(1),
            format: Some("image/png".into()),
            dpi: None,
        }
    }

    #[test]
    fn exceptions_absent_defaults_to_xml() {
        let parsed = ParsedGetMap {
            viewport: happy_viewport(),
            exceptions: None,
        };
        let r = resolve_get_map(parsed, &cfg(), WmsVersion::V130).unwrap();
        assert_eq!(r.exceptions, ExceptionsFormat::Xml);
    }

    #[test]
    fn exceptions_blank_keyword_accepted() {
        for kw in ["BLANK", "blank", "application/vnd.ogc.se_blank"] {
            let parsed = ParsedGetMap {
                viewport: happy_viewport(),
                exceptions: Some(kw.into()),
            };
            let r = resolve_get_map(parsed, &cfg(), WmsVersion::V130).unwrap();
            assert_eq!(r.exceptions, ExceptionsFormat::Blank, "kw={kw}");
        }
    }

    #[test]
    fn exceptions_inimage_not_implemented() {
        let parsed = ParsedGetMap {
            viewport: happy_viewport(),
            exceptions: Some("INIMAGE".into()),
        };
        let err = resolve_get_map(parsed, &cfg(), WmsVersion::V130).unwrap_err();
        assert!(matches!(err, WmsError::NotImplemented { .. }));
    }

    #[test]
    fn exceptions_unknown_rejected() {
        let parsed = ParsedGetMap {
            viewport: happy_viewport(),
            exceptions: Some("GARBAGE".into()),
        };
        let err = resolve_get_map(parsed, &cfg(), WmsVersion::V130).unwrap_err();
        assert!(matches!(err, WmsError::InvalidParam { name: "exceptions", .. }));
    }

    #[test]
    fn dpi_override_applied() {
        let mut vp = happy_viewport();
        vp.dpi = Some(72.0);
        let parsed = ParsedGetMap {
            viewport: vp,
            exceptions: None,
        };
        let r = resolve_get_map(parsed, &cfg(), WmsVersion::V130).unwrap();
        assert!((r.plan.scale_pixel_size_m - 0.0254 / 72.0).abs() < 1e-12);
    }

    #[test]
    fn missing_layers_reports_missing() {
        let mut vp = happy_viewport();
        vp.layers = None;
        let parsed = ParsedGetMap {
            viewport: vp,
            exceptions: None,
        };
        let err = resolve_get_map(parsed, &cfg(), WmsVersion::V130).unwrap_err();
        assert!(matches!(err, WmsError::MissingParam("layers")));
    }

    #[test]
    fn crs_not_in_allowlist_rejected() {
        let mut vp = happy_viewport();
        vp.crs = Some("EPSG:3857".into());
        let parsed = ParsedGetMap {
            viewport: vp,
            exceptions: None,
        };
        let err = resolve_get_map(parsed, &cfg(), WmsVersion::V130).unwrap_err();
        assert!(matches!(err, WmsError::InvalidParam { name: "crs", .. }));
    }
}
