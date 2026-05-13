//! `GetFeatureInfo` KVP extraction. Produces an Option-heavy
//! [`ParsedGetFeatureInfo`] consumed by
//! [`crate::prepare::resolve_get_feature_info`]; reuses
//! [`super::get_map::parse_viewport`] for the shared LAYERS/CRS/BBOX/...
//! slice so the two ops share one extractor and one validator.

use mars_types::LayerId;

use super::common::{Kvp, nonempty, parse_kvp, parse_optional_u32};
use super::get_map::parse_viewport;
use crate::prepare::{ParsedGetFeatureInfo, ResolvedGetFeatureInfo, resolve_get_feature_info};
use crate::{WmsConfig, WmsError};

/// Parse a `GetFeatureInfo` query-string into a [`ResolvedGetFeatureInfo`].
pub fn parse_get_feature_info(query: &str, cfg: &WmsConfig) -> Result<ResolvedGetFeatureInfo, WmsError> {
    let kvp = parse_kvp(query);
    let parsed = parse_get_feature_info_kvp(&kvp)?;
    resolve_get_feature_info(parsed, cfg)
}

pub(super) fn resolve_get_feature_info_from_kvp(
    kvp: &Kvp,
    cfg: &WmsConfig,
) -> Result<ResolvedGetFeatureInfo, WmsError> {
    let parsed = parse_get_feature_info_kvp(kvp)?;
    resolve_get_feature_info(parsed, cfg)
}

fn parse_get_feature_info_kvp(kvp: &Kvp) -> Result<ParsedGetFeatureInfo, WmsError> {
    Ok(ParsedGetFeatureInfo {
        viewport: parse_viewport(kvp)?,
        query_layers: parse_query_layers(kvp),
        i: parse_optional_u32(kvp, "i")?,
        j: parse_optional_u32(kvp, "j")?,
        info_format: nonempty(kvp, "info_format"),
        feature_count: parse_optional_u32(kvp, "feature_count")?,
    })
}

fn parse_query_layers(kvp: &Kvp) -> Option<Vec<LayerId>> {
    let raw = kvp.get("query_layers").filter(|s| !s.is_empty())?;
    Some(raw.split(',').filter(|s| !s.is_empty()).map(LayerId::new).collect())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use mars_types::{CrsCode, ImageFormat};

    use super::super::parse_request;
    use super::*;
    use crate::MAX_FEATURE_COUNT;

    fn cfg() -> WmsConfig {
        WmsConfig {
            allowlist_crs: vec![CrsCode::new("EPSG:25832"), CrsCode::new("EPSG:4326")],
            formats: vec![ImageFormat::Png],
            max_image_dimension: 8192,
            max_pixels: 16_000_000,
            max_layers: 100,
            max_bbox_coord: 1e9,
            scale_pixel_size_m: 0.0254 / 96.0,
        }
    }

    #[test]
    fn gfi_happy_path() {
        let q = "request=GetFeatureInfo&version=1.3.0&layers=a,b&styles=&crs=EPSG:25832&\
                 bbox=0,0,100,100&width=10&height=10&format=image/png&\
                 query_layers=a&info_format=text/plain&i=5&j=7";
        let gfi = parse_get_feature_info(q, &cfg()).unwrap();
        assert_eq!(gfi.plan.layers.len(), 1);
        assert_eq!(gfi.plan.layers[0].as_str(), "a");
        assert_eq!(gfi.i, 5);
        assert_eq!(gfi.j, 7);
        assert_eq!(gfi.info_format, crate::InfoFormat::TextPlain);
        assert_eq!(gfi.feature_count, 1);
    }

    #[test]
    fn gfi_query_layers_must_be_subset_of_layers() {
        let q = "request=GetFeatureInfo&version=1.3.0&layers=a&styles=&crs=EPSG:25832&\
                 bbox=0,0,100,100&width=10&height=10&format=image/png&\
                 query_layers=z&info_format=text/plain&i=0&j=0";
        let err = parse_request(q, &cfg()).unwrap_err();
        assert!(matches!(
            err,
            WmsError::InvalidParam {
                name: "query_layers",
                ..
            }
        ));
    }

    #[test]
    fn gfi_pixel_out_of_viewport_rejected() {
        let q = "request=GetFeatureInfo&version=1.3.0&layers=a&styles=&crs=EPSG:25832&\
                 bbox=0,0,100,100&width=10&height=10&format=image/png&\
                 query_layers=a&info_format=text/plain&i=10&j=0";
        let err = parse_request(q, &cfg()).unwrap_err();
        assert!(matches!(err, WmsError::InvalidParam { name: "i|j", .. }));
    }

    #[test]
    fn gfi_unsupported_info_format_rejected() {
        let q = "request=GetFeatureInfo&version=1.3.0&layers=a&styles=&crs=EPSG:25832&\
                 bbox=0,0,100,100&width=10&height=10&format=image/png&\
                 query_layers=a&info_format=application/vnd.ogc.gml&i=0&j=0";
        let err = parse_request(q, &cfg()).unwrap_err();
        assert!(matches!(
            err,
            WmsError::InvalidParam {
                name: "info_format",
                ..
            }
        ));
    }

    #[test]
    fn gfi_feature_count_clamped_to_max() {
        let q = format!(
            "request=GetFeatureInfo&version=1.3.0&layers=a&styles=&crs=EPSG:25832&\
             bbox=0,0,100,100&width=10&height=10&format=image/png&\
             query_layers=a&info_format=text/plain&i=0&j=0&feature_count={}",
            MAX_FEATURE_COUNT + 100
        );
        let gfi = parse_get_feature_info(&q, &cfg()).unwrap();
        assert_eq!(gfi.feature_count, MAX_FEATURE_COUNT);
    }

    #[test]
    fn gfi_feature_count_zero_rejected() {
        let q = "request=GetFeatureInfo&version=1.3.0&layers=a&styles=&crs=EPSG:25832&\
                 bbox=0,0,100,100&width=10&height=10&format=image/png&\
                 query_layers=a&info_format=text/plain&i=0&j=0&feature_count=0";
        let err = parse_request(q, &cfg()).unwrap_err();
        assert!(matches!(
            err,
            WmsError::InvalidParam {
                name: "feature_count",
                ..
            }
        ));
    }
}
