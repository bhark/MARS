//! GeoJSON decoder. Parses bytes via `serde_json`, walks the feature
//! collection, and lowers each feature's geometry to OGC WKB via
//! `geozero`. Property decoding mirrors RFC 7946: bool / number / string
//! / null become the corresponding [`AttrValue`]; objects and arrays
//! serialise back to JSON text.

use std::collections::HashMap;

use bytes::Bytes;
use geozero::{CoordDimensions, ToWkb};
use mars_config::VectorFileFormat;
use mars_source::AttrValue;
use serde_json::Value;

use super::{DecodedFeature, Decoder};
use crate::error::DecoderError;

/// Pure-Rust GeoJSON decoder. RFC 7946 FeatureCollection or single Feature.
pub struct GeoJsonDecoder;

impl Decoder for GeoJsonDecoder {
    fn name(&self) -> &'static str {
        "geojson"
    }

    fn supports(&self, format: VectorFileFormat) -> bool {
        matches!(format, VectorFileFormat::GeoJson)
    }

    fn decode(&self, bytes: &Bytes, sink: &mut dyn FnMut(DecodedFeature) -> bool) -> Result<(), DecoderError> {
        let root: Value = serde_json::from_slice(bytes).map_err(|e| DecoderError::Parse {
            format: "geojson",
            source: Box::new(e),
        })?;

        match root.get("type").and_then(|v| v.as_str()) {
            Some("FeatureCollection") => {
                let features = root
                    .get("features")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| DecoderError::Schema("FeatureCollection missing features[]".into()))?;
                for (idx, feat) in features.iter().enumerate() {
                    if !emit_feature(idx as u64, feat, sink)? {
                        break;
                    }
                }
            }
            Some("Feature") => {
                let _ = emit_feature(0, &root, sink)?;
            }
            other => {
                return Err(DecoderError::Schema(format!(
                    "unsupported geojson root type: {other:?}"
                )));
            }
        }
        Ok(())
    }
}

fn emit_feature(
    row_idx: u64,
    feat: &Value,
    sink: &mut dyn FnMut(DecodedFeature) -> bool,
) -> Result<bool, DecoderError> {
    // native fid: GeoJSON RFC 7946 allows a top-level `id`. integer ids
    // map to feature_id; non-integer ids fall back to row index.
    let feature_id = feat.get("id").and_then(value_to_u64).unwrap_or(0);

    let geom = feat
        .get("geometry")
        .ok_or_else(|| DecoderError::Schema(format!("feature {row_idx} missing geometry")))?;
    if geom.is_null() {
        // skip null-geom features; matches the postgis adapter's behaviour.
        return Ok(true);
    }
    let wkb = geometry_to_wkb(geom)?;

    let mut attributes = HashMap::new();
    if let Some(props) = feat.get("properties").and_then(|v| v.as_object()) {
        for (k, v) in props {
            attributes.insert(k.clone(), value_to_attr(v));
        }
    }

    Ok(sink(DecodedFeature {
        feature_id,
        geometry_wkb: Bytes::from(wkb),
        attributes,
    }))
}

fn geometry_to_wkb(geom: &Value) -> Result<Vec<u8>, DecoderError> {
    // round-trip through a string so we can reuse geozero's GeoJson reader
    // for the geom->wkb path. cheap relative to the proj transform that
    // follows downstream.
    let s = serde_json::to_string(geom).map_err(|e| DecoderError::Parse {
        format: "geojson",
        source: Box::new(e),
    })?;
    geozero::geojson::GeoJson(&s)
        .to_wkb(CoordDimensions::xy())
        .map_err(|e| DecoderError::Parse {
            format: "geojson",
            source: Box::new(e),
        })
}

fn value_to_attr(v: &Value) -> AttrValue {
    match v {
        Value::Null => AttrValue::Null,
        Value::Bool(b) => AttrValue::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                AttrValue::Int(i)
            } else if let Some(u) = n.as_u64() {
                AttrValue::Int(i64::try_from(u).unwrap_or(i64::MAX))
            } else if let Some(f) = n.as_f64() {
                AttrValue::Float(f)
            } else {
                AttrValue::Null
            }
        }
        Value::String(s) => AttrValue::String(s.clone()),
        // nested objects / arrays surface as their json string form so the
        // downstream expr layer can decide what to do with them. losing
        // structure is acceptable for the v1 attribute model (mars_expr
        // does not have a nested-value type).
        other => AttrValue::String(other.to_string()),
    }
}

fn value_to_u64(v: &Value) -> Option<u64> {
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    if let Some(i) = v.as_i64() {
        return u64::try_from(i).ok();
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn decodes_known_geojson() {
        let s = r#"{
            "type": "FeatureCollection",
            "features": [
              {
                "type": "Feature",
                "id": 7,
                "geometry": {"type":"Point","coordinates":[10.0,20.0]},
                "properties": {"name":"alpha", "count": 42, "active": true}
              },
              {
                "type": "Feature",
                "geometry": {"type":"Point","coordinates":[1.0,2.0]},
                "properties": {"name":"beta", "count": 1.5}
              }
            ]
        }"#;
        let mut got: Vec<DecodedFeature> = Vec::new();
        GeoJsonDecoder
            .decode(&Bytes::from(s.as_bytes().to_vec()), &mut |f| {
                got.push(f);
                true
            })
            .unwrap();
        assert_eq!(got.len(), 2);
        // first feature has explicit id
        assert_eq!(got[0].feature_id, 7);
        assert_eq!(got[1].feature_id, 0); // fallback to row index downstream

        // wkb sanity
        assert!(!got[0].geometry_wkb.is_empty());
        assert_eq!(got[0].geometry_wkb[0], 1); // little-endian

        // attribute decoding
        assert!(matches!(got[0].attributes.get("name"), Some(AttrValue::String(s)) if s == "alpha"));
        assert!(matches!(got[0].attributes.get("count"), Some(AttrValue::Int(42))));
        assert!(matches!(got[0].attributes.get("active"), Some(AttrValue::Bool(true))));
        assert!(matches!(got[1].attributes.get("count"), Some(AttrValue::Float(f)) if (*f - 1.5).abs() < 1e-9));
    }

    #[test]
    fn skips_null_geometry() {
        let s = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","geometry":null,"properties":{}}
        ]}"#;
        let mut got = 0;
        GeoJsonDecoder
            .decode(&Bytes::from(s.as_bytes().to_vec()), &mut |_| {
                got += 1;
                true
            })
            .unwrap();
        assert_eq!(got, 0);
    }

    #[test]
    fn single_feature_root() {
        let s = r#"{"type":"Feature","id":3,"geometry":{"type":"Point","coordinates":[0,0]},"properties":{}}"#;
        let mut got: Vec<DecodedFeature> = Vec::new();
        GeoJsonDecoder
            .decode(&Bytes::from(s.as_bytes().to_vec()), &mut |f| {
                got.push(f);
                true
            })
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].feature_id, 3);
    }
}
