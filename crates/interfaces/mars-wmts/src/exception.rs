//! OWS 1.1 ExceptionReport XML builder. WMTS 1.0.0 (OGC 07-057r7) reuses the
//! OWS Common ExceptionReport schema rather than the WMS ServiceException
//! shape, so a strict WMTS client expects this envelope on errors.

use std::io::Cursor;

use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};

/// Build an `ExceptionReport` document. `locator` identifies the parameter
/// the exception relates to; OWS allows it to be omitted.
#[must_use]
#[allow(clippy::expect_used)] // writing to Vec<u8> is infallible
pub fn ows_exception_report(code: &str, locator: Option<&str>, message: &str) -> String {
    let mut buf = Cursor::new(Vec::new());
    let mut w = Writer::new(&mut buf);

    w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("infallible write to Vec<u8>");

    let mut root = BytesStart::new("ExceptionReport");
    root.push_attribute(("xmlns", "http://www.opengis.net/ows/1.1"));
    root.push_attribute(("version", "1.1.0"));
    root.push_attribute(("xml:lang", "en"));
    w.write_event(Event::Start(root)).expect("infallible write to Vec<u8>");

    let mut exc = BytesStart::new("Exception");
    exc.push_attribute(("exceptionCode", code));
    if let Some(loc) = locator {
        exc.push_attribute(("locator", loc));
    }
    w.write_event(Event::Start(exc)).expect("infallible write to Vec<u8>");
    w.write_event(Event::Start(BytesStart::new("ExceptionText")))
        .expect("infallible write to Vec<u8>");
    w.write_event(Event::Text(BytesText::new(message)))
        .expect("infallible write to Vec<u8>");
    w.write_event(Event::End(BytesEnd::new("ExceptionText")))
        .expect("infallible write to Vec<u8>");
    w.write_event(Event::End(BytesEnd::new("Exception")))
        .expect("infallible write to Vec<u8>");

    w.write_event(Event::End(BytesEnd::new("ExceptionReport")))
        .expect("infallible write to Vec<u8>");

    String::from_utf8(buf.into_inner()).expect("xml is valid utf-8")
}

#[cfg(test)]
mod tests;
