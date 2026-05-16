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
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_fallback() {
        let out = configured_formats(&[], ImageFormat::Png);
        assert_eq!(out, vec![ImageFormat::Png]);
    }

    #[test]
    fn only_unknown_mimes_returns_fallback() {
        let out = configured_formats(&["application/xyz".into()], ImageFormat::Png);
        assert_eq!(out, vec![ImageFormat::Png]);
    }

    #[test]
    fn parses_known_mimes_in_order() {
        let out = configured_formats(
            &["image/jpeg".into(), "image/png".into(), "unknown".into()],
            ImageFormat::Png,
        );
        assert_eq!(out, vec![ImageFormat::Jpeg, ImageFormat::Png]);
    }
}
