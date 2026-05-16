//! URI-fragment encoding for vector-file bindings.
//!
//! The port-level `SourceBinding.from` is a single opaque string. To carry
//! both the file's URI and the per-binding decoder hint + source CRS
//! through that one string, this adapter uses a URL-fragment convention:
//!
//! ```text
//! <uri>[#format=<flat_geobuf|geo_json>&source_crs=<EPSG:XXXX>]
//! ```
//!
//! The fragment is optional. When omitted, [`parse`] infers the format
//! from the URI's file extension (`.fgb` -> FlatGeobuf, `.geojson|.json`
//! -> GeoJson) and leaves `source_crs` unset; the caller falls back to
//! [`mars_source::SourceBinding::crs`].
//!
//! The composition layer that builds port bindings from the typed
//! `mars_config::SourceBinding` is responsible for formatting the
//! fragment when both `format:` and `source_crs:` are present in config.

use mars_config::VectorFileFormat;
use mars_types::CrsCode;

/// Parsed locator: URI body plus optional decoder hint and source CRS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLocator {
    /// URI body with the fragment stripped (the value passed to object_store).
    pub uri: String,
    /// Decoder hint. Inferred from extension when the fragment omits it.
    pub format: VectorFileFormat,
    /// Source CRS, when carried by the fragment. `None` means the caller
    /// should fall back to the binding-level `crs` field.
    pub source_crs: Option<CrsCode>,
}

/// Parse error.
#[derive(Debug, thiserror::Error)]
pub enum FragmentError {
    /// Fragment carried a `format=` whose value is not a known variant.
    #[error("unknown format hint: {0:?}")]
    UnknownFormat(String),
    /// Locator carries no fragment AND the URI extension does not encode
    /// a recognised format.
    #[error("could not infer format from uri (use #format=...): {0}")]
    UndecidableFormat(String),
    /// Fragment carried a key the parser does not understand.
    #[error("unknown fragment key: {0}")]
    UnknownKey(String),
    /// Fragment key was present but had no value.
    #[error("empty value for fragment key {0}")]
    EmptyValue(&'static str),
}

/// Parse a locator string into URI + decoder hint + optional source CRS.
pub fn parse(locator: &str) -> Result<ParsedLocator, FragmentError> {
    let (uri_part, frag_part) = match locator.split_once('#') {
        Some((u, f)) => (u, Some(f)),
        None => (locator, None),
    };

    let mut format: Option<VectorFileFormat> = None;
    let mut source_crs: Option<CrsCode> = None;

    if let Some(frag) = frag_part {
        for pair in frag.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            match k {
                "format" => {
                    if v.is_empty() {
                        return Err(FragmentError::EmptyValue("format"));
                    }
                    format = Some(parse_format(v)?);
                }
                "source_crs" => {
                    if v.is_empty() {
                        return Err(FragmentError::EmptyValue("source_crs"));
                    }
                    source_crs = Some(CrsCode::new(v));
                }
                other => return Err(FragmentError::UnknownKey(other.to_string())),
            }
        }
    }

    let format = match format {
        Some(f) => f,
        None => infer_from_extension(uri_part).ok_or_else(|| FragmentError::UndecidableFormat(uri_part.to_string()))?,
    };

    Ok(ParsedLocator {
        uri: uri_part.to_string(),
        format,
        source_crs,
    })
}

fn parse_format(s: &str) -> Result<VectorFileFormat, FragmentError> {
    match s {
        "flat_geobuf" | "flatgeobuf" | "fgb" => Ok(VectorFileFormat::FlatGeobuf),
        "geo_json" | "geojson" | "json" => Ok(VectorFileFormat::GeoJson),
        other => Err(FragmentError::UnknownFormat(other.to_string())),
    }
}

fn infer_from_extension(uri: &str) -> Option<VectorFileFormat> {
    // last '.' after the last '/' or end. case-insensitive.
    let tail = match uri.rsplit_once('/') {
        Some((_, t)) => t,
        None => uri,
    };
    let ext = tail.rsplit_once('.').map(|(_, e)| e)?;
    let lower = ext.to_ascii_lowercase();
    match lower.as_str() {
        "fgb" => Some(VectorFileFormat::FlatGeobuf),
        "geojson" | "json" => Some(VectorFileFormat::GeoJson),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn parses_format_and_crs_from_fragment() {
        let p = parse("file:///x.fgb#format=flat_geobuf&source_crs=EPSG:4326").unwrap();
        assert_eq!(p.uri, "file:///x.fgb");
        assert_eq!(p.format, VectorFileFormat::FlatGeobuf);
        assert_eq!(p.source_crs.unwrap().as_str(), "EPSG:4326");
    }

    #[test]
    fn falls_back_to_extension_inference() {
        let p = parse("s3://bucket/data/roads.fgb").unwrap();
        assert_eq!(p.uri, "s3://bucket/data/roads.fgb");
        assert_eq!(p.format, VectorFileFormat::FlatGeobuf);
        assert!(p.source_crs.is_none());

        let p = parse("https://example.org/data.geojson").unwrap();
        assert_eq!(p.format, VectorFileFormat::GeoJson);
    }

    #[test]
    fn empty_fragment_value_rejected() {
        let err = parse("file:///x.fgb#format=").unwrap_err();
        assert!(matches!(err, FragmentError::EmptyValue("format")));
    }

    #[test]
    fn unknown_key_rejected() {
        let err = parse("file:///x.fgb#weird=1").unwrap_err();
        assert!(matches!(err, FragmentError::UnknownKey(k) if k == "weird"));
    }

    #[test]
    fn undecidable_extension_rejected() {
        let err = parse("file:///opaque").unwrap_err();
        assert!(matches!(err, FragmentError::UndecidableFormat(_)));
    }

    #[test]
    fn accepts_alternate_spellings() {
        assert_eq!(parse("u#format=fgb").unwrap().format, VectorFileFormat::FlatGeobuf);
        assert_eq!(parse("u#format=geo_json").unwrap().format, VectorFileFormat::GeoJson);
    }
}
