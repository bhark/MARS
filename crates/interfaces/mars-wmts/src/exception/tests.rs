#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn includes_code_and_message() {
    let xml = ows_exception_report("InvalidParameterValue", Some("LAYER"), "bad layer");
    assert!(xml.contains(r#"exceptionCode="InvalidParameterValue""#));
    assert!(xml.contains(r#"locator="LAYER""#));
    assert!(xml.contains("<ExceptionText>bad layer</ExceptionText>"));
    assert!(xml.contains("</ExceptionReport>"));
}

#[test]
fn omits_locator_when_none() {
    let xml = ows_exception_report("OperationNotSupported", None, "no GFI");
    assert!(!xml.contains("locator="));
}

#[test]
fn escapes_xml_special_chars() {
    let xml = ows_exception_report("X", None, "a & b <c>");
    assert!(!xml.contains("a & b <c>"));
    assert!(xml.contains("a &amp; b &lt;c&gt;"));
}

#[test]
fn declares_ows_namespace() {
    let xml = ows_exception_report("X", None, "y");
    assert!(xml.contains("http://www.opengis.net/ows/1.1"));
    // strict clients reject WMS-shaped reports here
    assert!(!xml.contains("ServiceExceptionReport"));
}
