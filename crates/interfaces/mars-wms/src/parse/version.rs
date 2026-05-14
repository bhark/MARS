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

use super::common::Kvp;
use crate::{WmsError, WmsVersion};

/// Strict negotiation used on the request path. Reads `version` / `wmtver`
/// from `kvp` and returns the matching [`WmsVersion`]. Missing or empty
/// resolves to the default version.
pub(super) fn negotiate_version(kvp: &Kvp) -> Result<WmsVersion, WmsError> {
    let raw = kvp
        .get("version")
        .or_else(|| kvp.get("wmtver"))
        .map(String::as_str)
        .filter(|s| !s.is_empty());
    let Some(raw) = raw else {
        return Ok(WmsVersion::default());
    };
    match raw {
        "1.3.0" => Ok(WmsVersion::V130),
        other => Err(WmsError::InvalidParam {
            name: "version",
            reason: format!("unsupported `{other}` (server speaks 1.3.0)"),
        }),
    }
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
    fn unsupported_111_rejected_for_now() {
        let kvp = parse_kvp("request=GetCapabilities&version=1.1.1");
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
}
