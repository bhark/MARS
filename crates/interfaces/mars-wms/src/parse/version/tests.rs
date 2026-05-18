#![allow(clippy::unwrap_used)]

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
