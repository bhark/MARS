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
//! -> GeoJson, `.shp.zip|.shz` -> Shapefile) and leaves `source_crs`
//! unset; the caller falls back to
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
    /// Raw `.shp` URI (or a forced shapefile hint over a non-archive URI).
    /// Shapefile is multi-file by definition and the adapter only accepts
    /// the bundled `.shp.zip` / `.shz` carrier; the underlying decoder
    /// requires `.shp` + `.shx` + `.dbf` together, so the triple must be
    /// packaged before fetch.
    #[error(
        "raw shapefile triples are not supported; bundle .shp/.shx/.dbf (and optional .prj) into a .shp.zip or .shz: {0}"
    )]
    UnsupportedRawShapefile(String),
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

    // shapefile is multi-file: only the .shp.zip / .shz archive is supported.
    // a raw .shp URI (inferred or forced) cannot be decoded since .shx + .dbf
    // sidecars are required by the upstream reader.
    if format == VectorFileFormat::Shapefile && !has_shapefile_archive_ext(uri_part) {
        return Err(FragmentError::UnsupportedRawShapefile(uri_part.to_string()));
    }

    Ok(ParsedLocator {
        uri: uri_part.to_string(),
        format,
        source_crs,
    })
}

fn has_shapefile_archive_ext(uri: &str) -> bool {
    let tail = match uri.rsplit_once('/') {
        Some((_, t)) => t,
        None => uri,
    };
    let lower = tail.to_ascii_lowercase();
    lower.ends_with(".shp.zip") || lower.ends_with(".shz")
}

fn parse_format(s: &str) -> Result<VectorFileFormat, FragmentError> {
    match s {
        "flat_geobuf" | "flatgeobuf" | "fgb" => Ok(VectorFileFormat::FlatGeobuf),
        "geo_json" | "geojson" | "json" => Ok(VectorFileFormat::GeoJson),
        "shapefile" | "shp" | "shz" => Ok(VectorFileFormat::Shapefile),
        "geo_package" | "geopackage" | "gpkg" => Ok(VectorFileFormat::GeoPackage),
        other => Err(FragmentError::UnknownFormat(other.to_string())),
    }
}

fn infer_from_extension(uri: &str) -> Option<VectorFileFormat> {
    // last '.' after the last '/' or end. case-insensitive.
    let tail = match uri.rsplit_once('/') {
        Some((_, t)) => t,
        None => uri,
    };
    let lower_tail = tail.to_ascii_lowercase();
    // shapefile lives in a compound extension; check it before the
    // single-extension cases so plain `.zip` doesn't masquerade.
    if lower_tail.ends_with(".shp.zip") || lower_tail.ends_with(".shz") {
        return Some(VectorFileFormat::Shapefile);
    }
    let ext = lower_tail.rsplit_once('.').map(|(_, e)| e)?;
    match ext {
        "fgb" => Some(VectorFileFormat::FlatGeobuf),
        "geojson" | "json" => Some(VectorFileFormat::GeoJson),
        "gpkg" => Some(VectorFileFormat::GeoPackage),
        // raw .shp surfaces as shapefile so the downstream archive check
        // can return UnsupportedRawShapefile with a precise message.
        "shp" => Some(VectorFileFormat::Shapefile),
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
        // shapefile spellings only pass when the URI is a recognised archive.
        assert_eq!(
            parse("u.shp.zip#format=shp").unwrap().format,
            VectorFileFormat::Shapefile
        );
        assert_eq!(
            parse("u.shz#format=shapefile").unwrap().format,
            VectorFileFormat::Shapefile
        );
    }

    #[test]
    fn infers_shapefile_compound_extension() {
        let p = parse("s3://b/data/roads.shp.zip").unwrap();
        assert_eq!(p.format, VectorFileFormat::Shapefile);
        let p = parse("file:///x.shz").unwrap();
        assert_eq!(p.format, VectorFileFormat::Shapefile);
    }

    #[test]
    fn rejects_raw_shp_uri() {
        let err = parse("s3://bucket/data/roads.shp").unwrap_err();
        match err {
            FragmentError::UnsupportedRawShapefile(uri) => {
                assert_eq!(uri, "s3://bucket/data/roads.shp");
            }
            other => panic!("expected UnsupportedRawShapefile, got {other:?}"),
        }
    }

    #[test]
    fn rejects_forced_shapefile_on_non_archive_uri() {
        let err = parse("file:///opaque.bin#format=shapefile").unwrap_err();
        assert!(matches!(err, FragmentError::UnsupportedRawShapefile(_)));
    }
}
