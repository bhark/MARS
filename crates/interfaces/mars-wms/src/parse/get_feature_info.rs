//! `GetFeatureInfo` KVP parsing. Builds on the GetMap viewport parser and
//! layers in the GFI-specific inputs (query layers, pixel hit, info format,
//! feature count).

use mars_types::LayerId;

use super::common::{Kvp, parse_kvp, parse_u32, require};
use super::get_map::parse_get_map_inner;
use crate::feature_info::info_format_mime;
use crate::{GfiPlan, MAX_FEATURE_COUNT, WmsConfig, WmsError};

/// Parse a `GetFeatureInfo` query-string into a [`GfiPlan`].
pub fn parse_get_feature_info(query: &str, cfg: &WmsConfig) -> Result<GfiPlan, WmsError> {
    let kvp = parse_kvp(query);
    parse_get_feature_info_inner(&kvp, cfg)
}

pub(super) fn parse_get_feature_info_inner(kvp: &Kvp, cfg: &WmsConfig) -> Result<GfiPlan, WmsError> {
    // viewport params (layers, crs, bbox, width, height, format) reuse the
    // same parsing path as GetMap; this keeps allowlist + bound semantics in
    // one place and means anything new added to GetMap pickup is free here.
    let mut plan = parse_get_map_inner(kvp, cfg)?;

    let query_layers_raw = require(kvp, "query_layers")?;
    let query_layers: Vec<LayerId> = query_layers_raw
        .split(',')
        .filter(|s| !s.is_empty())
        .map(LayerId::new)
        .collect();
    if query_layers.is_empty() {
        return Err(WmsError::InvalidParam {
            name: "query_layers",
            reason: "no layer names".into(),
        });
    }
    // spec: QUERY_LAYERS must be a subset of LAYERS.
    for q in &query_layers {
        if !plan.layers.iter().any(|l| l == q) {
            return Err(WmsError::InvalidParam {
                name: "query_layers",
                reason: format!("`{}` is not in LAYERS", q.as_str()),
            });
        }
    }
    // gfi runs only against query layers; swap them in so the runtime walks
    // exactly those bindings.
    plan.layers = query_layers;

    let i = parse_u32(kvp, "i")?;
    let j = parse_u32(kvp, "j")?;
    if i >= plan.width || j >= plan.height {
        return Err(WmsError::InvalidParam {
            name: "i|j",
            reason: format!("({i},{j}) outside viewport {}x{}", plan.width, plan.height),
        });
    }

    let info_format_raw = require(kvp, "info_format")?;
    let info_format = info_format_mime(&info_format_raw).ok_or(WmsError::InvalidParam {
        name: "info_format",
        reason: format!("unsupported `{info_format_raw}`"),
    })?;

    let feature_count = parse_feature_count(kvp)?;

    Ok(GfiPlan {
        plan,
        i,
        j,
        info_format,
        feature_count,
    })
}

fn parse_feature_count(kvp: &Kvp) -> Result<u32, WmsError> {
    let raw = match kvp.get("feature_count") {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(1),
    };
    let n: u32 = raw
        .parse()
        .map_err(|e: std::num::ParseIntError| WmsError::InvalidParam {
            name: "feature_count",
            reason: e.to_string(),
        })?;
    if n == 0 {
        return Err(WmsError::InvalidParam {
            name: "feature_count",
            reason: "must be >= 1".into(),
        });
    }
    Ok(n.min(MAX_FEATURE_COUNT))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use mars_types::{CrsCode, ImageFormat};

    use super::super::parse_request;
    use super::*;

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
