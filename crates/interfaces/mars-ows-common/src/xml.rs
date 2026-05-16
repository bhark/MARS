//! XML emit primitives shared by OWS capabilities builders. The actual
//! element shapes (KeywordList, OnlineResource, DCPType, ...) stay in their
//! protocol crates because their spelling diverges per spec; what's lifted
//! here is the generic event-wrapping that every emitter needed and was
//! re-implementing identically.

use quick_xml::Writer;
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};

use crate::OwsParseError;

/// Wrap a [`quick_xml`] write error in the caller's parse-error type using
/// the `"capabilities"` parameter name. Matches the encoding both interface
/// crates used before consolidation.
pub fn xml_err<E: OwsParseError>(e: std::io::Error) -> E {
    E::invalid("capabilities", e.to_string())
}

/// Emit a `<name>text</name>` element with no attributes. The most common
/// shape in any capabilities document; consolidating it avoids three
/// near-identical `write_event` calls at each callsite.
pub fn text_element<W: std::io::Write, E: OwsParseError>(w: &mut Writer<W>, name: &str, text: &str) -> Result<(), E> {
    w.write_event(Event::Start(BytesStart::new(name))).map_err(xml_err)?;
    w.write_event(Event::Text(BytesText::new(text))).map_err(xml_err)?;
    w.write_event(Event::End(BytesEnd::new(name))).map_err(xml_err)?;
    Ok(())
}
