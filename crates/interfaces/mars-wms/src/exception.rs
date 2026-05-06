//! WMS 1.3.0 ServiceExceptionReport XML builder.
//! SPEC §7.4.

use std::io::Cursor;

use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};

/// Build a `ServiceExceptionReport` XML document.
#[must_use]
#[allow(clippy::expect_used)] // writing to Vec<u8> is infallible
pub fn service_exception_report(code: Option<&str>, message: &str) -> String {
    let mut buf = Cursor::new(Vec::new());
    let mut w = Writer::new(&mut buf);

    w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("infallible write to Vec<u8>");

    let mut root = BytesStart::new("ServiceExceptionReport");
    root.push_attribute(("version", "1.3.0"));
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
        let xml = service_exception_report(Some("InvalidParameterValue"), "bad bbox");
        assert!(xml.contains(r#"<ServiceException code="InvalidParameterValue">"#));
        assert!(xml.contains("bad bbox"));
        assert!(xml.contains("</ServiceExceptionReport>"));
    }

    #[test]
    fn omits_code_when_none() {
        let xml = service_exception_report(None, "generic error");
        assert!(!xml.contains("code="));
        assert!(xml.contains("<ServiceException>"));
        assert!(xml.contains("generic error"));
    }

    #[test]
    fn escapes_xml_special_chars() {
        let xml = service_exception_report(Some("X"), "a & b <c>");
        assert!(!xml.contains("a & b <c>"));
        assert!(xml.contains("a &amp; b &lt;c&gt;"));
    }
}
