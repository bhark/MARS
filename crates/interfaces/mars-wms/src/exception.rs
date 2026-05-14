//! WMS ServiceExceptionReport XML builder. Same envelope for both 1.1.1 and
//! 1.3.0 protocol versions; only the root `version=` attribute differs.

use std::io::Cursor;

use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};

use crate::WmsVersion;

/// Build a `ServiceExceptionReport` XML document tagged with the negotiated
/// WMS protocol version.
#[must_use]
#[allow(clippy::expect_used)] // writing to Vec<u8> is infallible
pub fn service_exception_report(version: WmsVersion, code: Option<&str>, message: &str) -> String {
    let mut buf = Cursor::new(Vec::new());
    let mut w = Writer::new(&mut buf);

    w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("infallible write to Vec<u8>");

    let mut root = BytesStart::new("ServiceExceptionReport");
    root.push_attribute(("version", version.as_str()));
    root.push_attribute(("xmlns", "http://www.opengis.net/ogc"));
    w.write_event(Event::Start(root)).expect("infallible write to Vec<u8>");

    let mut exc = BytesStart::new("ServiceException");
    if let Some(c) = code {
        exc.push_attribute(("code", c));
    }
    w.write_event(Event::Start(exc)).expect("infallible write to Vec<u8>");
    w.write_event(Event::Text(BytesText::new(message)))
        .expect("infallible write to Vec<u8>");
    w.write_event(Event::End(BytesEnd::new("ServiceException")))
        .expect("infallible write to Vec<u8>");

    w.write_event(Event::End(BytesEnd::new("ServiceExceptionReport")))
        .expect("infallible write to Vec<u8>");

    String::from_utf8(buf.into_inner()).expect("xml is valid utf-8")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
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
    fn escapes_xml_special_chars() {
        let xml = service_exception_report(WmsVersion::V130, Some("X"), "a & b <c>");
        assert!(!xml.contains("a & b <c>"));
        assert!(xml.contains("a &amp; b &lt;c&gt;"));
    }

    #[test]
    fn root_version_matches_negotiated() {
        let xml_130 = service_exception_report(WmsVersion::V130, None, "x");
        let xml_111 = service_exception_report(WmsVersion::V111, None, "x");
        assert!(xml_130.contains(r#"version="1.3.0""#));
        assert!(xml_111.contains(r#"version="1.1.1""#));
    }
}
