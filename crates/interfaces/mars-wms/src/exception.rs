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
mod tests;
