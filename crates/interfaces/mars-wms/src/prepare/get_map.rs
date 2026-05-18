//! GetMap normalisation: takes a [`super::ParsedGetMap`] and produces a
//! validated [`ResolvedGetMap`]. Composes [`super::viewport::resolve_viewport`]
//! and folds the EXCEPTIONS= default in one place.

use mars_runtime::RenderPlan;

use super::ParsedGetMap;
use super::viewport::resolve_viewport;
use mars_config::ServiceOp;

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
    let plan = resolve_viewport(&p.viewport, cfg, version, ServiceOp::WmsGetMap)?;
    let exceptions = resolve_exceptions(p.exceptions.as_deref())?;
    Ok(ResolvedGetMap { plan, exceptions })
}

/// `&EXCEPTIONS=` per OGC. Optional; defaults to XML when absent. Accepts
/// the bare keyword forms 1.3.0 servers emit (`XML`, `BLANK`, `INIMAGE`)
/// alongside the longer 1.1.1 MIME forms (`application/vnd.ogc.se_*`) so
/// clients of either era round-trip.
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
        Ok(ExceptionsFormat::Inimage)
    } else {
        Err(WmsError::InvalidParam {
            name: "exceptions",
            reason: format!("unsupported value `{raw}`"),
        })
    }
}

#[cfg(test)]
mod tests;
