#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn includes_code_and_message() {
    let xml = service_exception_report(WmsVersion::V130, Some("InvalidParameterValue"), "bad bbox");
    assert!(xml.contains(r#"<ServiceException code="InvalidParameterValue">"#));
    assert!(xml.contains("bad bbox"));
    assert!(xml.contains("</ServiceExceptionReport>"));
}

#[test]
fn omits_code_when_none() {
    let xml = service_exception_report(WmsVersion::V130, None, "generic error");
    assert!(!xml.contains("code="));
    assert!(xml.contains("<ServiceException>"));
    assert!(xml.contains("generic error"));
}

#[test]
fn escapes_special_chars_per_version() {
    // escaping must not depend on the negotiated version; assert both.
    for version in [WmsVersion::V111, WmsVersion::V130] {
        let xml = service_exception_report(version, Some("X"), "a & b <c>");
        assert!(!xml.contains("a & b <c>"), "{}", version.as_str());
        assert!(xml.contains("a &amp; b &lt;c&gt;"), "{}", version.as_str());
    }
}

#[test]
fn version_attribute_and_namespace_per_version() {
    // root carries the negotiated version and the ogc namespace
    // unconditionally; the latter is required by the spec on both 1.1.1
    // and 1.3.0 ServiceExceptionReport envelopes.
    for version in [WmsVersion::V111, WmsVersion::V130] {
        let xml = service_exception_report(version, None, "x");
        let expected = format!(r#"version="{}""#, version.as_str());
        assert!(xml.contains(&expected), "{}: {xml}", version.as_str());
        assert!(
            xml.contains(r#"xmlns="http://www.opengis.net/ogc""#),
            "{}: {xml}",
            version.as_str()
        );
    }
}
