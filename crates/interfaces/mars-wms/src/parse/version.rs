//! WMS version negotiation. Accepts `version=<x.y.z>` and the legacy
//! `wmtver=<x.y.z>` alias that pre-1.1.0 clients still emit.
//!
//! The strict path rejects anything outside the supported set with
//! [`WmsError::InvalidParam`]; missing/empty defaults to [`WmsVersion::default`]
//! per OGC GetCapabilities convention (server picks its highest supported
//! version when the client did not specify one).
//!
//! 1.1.1 acceptance lands in the same commit that wires the per-version
//! parse forks. Until then the strict path keeps the prior single-version
//! behaviour intact.

use super::common::{Kvp, parse_kvp};
use crate::{WmsError, WmsVersion};

/// Strict negotiation used on the request path. Reads `version` / `wmtver`
/// from `kvp` and returns the matching [`WmsVersion`]. Missing or empty
/// resolves to the default version.
pub(super) fn negotiate_version(kvp: &Kvp) -> Result<WmsVersion, WmsError> {
    let raw = lookup_version(kvp);
    let Some(raw) = raw else {
        return Ok(WmsVersion::default());
    };
    match raw {
        "1.1.1" => Ok(WmsVersion::V111),
        "1.3.0" => Ok(WmsVersion::V130),
        other => Err(WmsError::InvalidParam {
            name: "version",
            reason: format!("unsupported `{other}` (server speaks 1.1.1 and 1.3.0)"),
        }),
    }
}

/// Lenient negotiation used by error-response formatting. Never fails:
/// returns the closest supported version for the requested wire string,
/// falling back to [`WmsVersion::default`] for missing / unknown / malformed
/// inputs. Used by the HTTP edge so a request that fails to parse can still
/// be answered in the version the client appears to have asked for.
#[must_use]
pub fn version_for_error_response(query: &str) -> WmsVersion {
    let kvp = parse_kvp(query);
    lookup_version(&kvp)
        .and_then(|raw| match raw {
            "1.1.1" => Some(WmsVersion::V111),
            "1.3.0" => Some(WmsVersion::V130),
            _ => None,
        })
        .unwrap_or_default()
}

fn lookup_version(kvp: &Kvp) -> Option<&str> {
    kvp.get("version")
        .or_else(|| kvp.get("wmtver"))
        .map(String::as_str)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::common::parse_kvp;
    use super::*;

    #[test]
    fn missing_version_defaults_to_default() {
        let kvp = parse_kvp("request=GetCapabilities");
        assert_eq!(negotiate_version(&kvp).unwrap(), WmsVersion::default());
    }

    #[test]
    fn empty_version_defaults_to_default() {
        let kvp = parse_kvp("request=GetCapabilities&version=");
        assert_eq!(negotiate_version(&kvp).unwrap(), WmsVersion::default());
    }

    #[test]
    fn explicit_130_resolves() {
        let kvp = parse_kvp("request=GetCapabilities&version=1.3.0");
        assert_eq!(negotiate_version(&kvp).unwrap(), WmsVersion::V130);
    }

    #[test]
    fn explicit_111_resolves() {
        let kvp = parse_kvp("request=GetCapabilities&version=1.1.1");
        assert_eq!(negotiate_version(&kvp).unwrap(), WmsVersion::V111);
    }

    #[test]
    fn unsupported_version_rejected() {
        let kvp = parse_kvp("request=GetCapabilities&version=1.0.0");
        assert!(matches!(
            negotiate_version(&kvp).unwrap_err(),
            WmsError::InvalidParam { name: "version", .. }
        ));
    }

    #[test]
    fn wmtver_alias_accepted() {
        let kvp = parse_kvp("request=GetCapabilities&wmtver=1.3.0");
        assert_eq!(negotiate_version(&kvp).unwrap(), WmsVersion::V130);
    }

    #[test]
    fn lenient_returns_default_on_unknown() {
        // strict path would reject; lenient path silently picks the default
        // so the error response carries the server's preferred version.
        assert_eq!(
            version_for_error_response("request=GetMap&version=garbage"),
            WmsVersion::default()
        );
    }

    #[test]
    fn lenient_returns_explicit_130() {
        assert_eq!(version_for_error_response("version=1.3.0"), WmsVersion::V130);
    }

    #[test]
    fn lenient_handles_missing() {
        assert_eq!(version_for_error_response("request=GetMap"), WmsVersion::default());
    }
}
