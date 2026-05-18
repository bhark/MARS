//! Output-format negotiation shared by capabilities builders. Resolves a
//! configured list of MIME strings into typed [`ImageFormat`]s, falling
//! back to a caller-supplied default when nothing parses (legacy WMS/WMTS
//! behaviour: PNG).

use mars_types::ImageFormat;

/// Resolve `configured` (a list of MIME strings) into [`ImageFormat`]s.
/// Unknown MIMEs are silently dropped; if the result is empty, `fallback`
/// is returned as a single-element vec.
#[must_use]
pub fn configured_formats(configured: &[String], fallback: ImageFormat) -> Vec<ImageFormat> {
    let parsed: Vec<ImageFormat> = configured
        .iter()
        .filter_map(|f| ImageFormat::from_mime(f.as_str()))
        .collect();
    if parsed.is_empty() { vec![fallback] } else { parsed }
}

#[cfg(test)]
mod tests;
