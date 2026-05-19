//! FlatGeobuf decoder. Sequential scan via the `flatgeobuf` reader; WKB
//! emission via `geozero`'s `ToWkb`. The packed R-tree index is not
//! used in v1 (the compiler doesn't drive spatial filters into the
//! source layer yet).

use std::collections::HashMap;
use std::io::Cursor;

use bytes::Bytes;
use fallible_streaming_iterator::FallibleStreamingIterator;
use flatgeobuf::{FeatureProperties, FgbReader};
use geozero::{CoordDimensions, PropertyProcessor, ToWkb};
use mars_config::VectorFileFormat;
use mars_source::AttrValue;

use super::{DecodedFeature, Decoder};
use crate::error::DecoderError;

/// FlatGeobuf decoder. Bytes -> per-feature `DecodedFeature`.
pub struct FlatGeobufDecoder;

impl Decoder for FlatGeobufDecoder {
    fn name(&self) -> &'static str {
        "flatgeobuf"
    }

    fn supports(&self, format: VectorFileFormat) -> bool {
        matches!(format, VectorFileFormat::FlatGeobuf)
    }

    fn decode(&self, bytes: &Bytes, sink: &mut dyn FnMut(DecodedFeature) -> bool) -> Result<(), DecoderError> {
        let cursor = Cursor::new(bytes.clone());
        let reader = FgbReader::open(cursor).map_err(parse_err)?;
        let header_has_z = reader.header().has_z();
        if header_has_z {
            return Err(DecoderError::Schema(
                "flatgeobuf has Z dimension; v1 emits xy wkb only".into(),
            ));
        }

        let mut iter = reader.select_all().map_err(parse_err)?;
        let mut row_idx: u64 = 0;
        while let Some(feat) = iter.next().map_err(parse_err)? {
            let wkb = feat.to_wkb(CoordDimensions::xy()).map_err(|e| DecoderError::Parse {
                format: "flatgeobuf",
                source: Box::new(e),
            })?;

            let mut collector = PropertyCollector::new();
            // process_properties drives the PropertyProcessor we pass; this
            // path does not call any geometry callbacks, so it's cheap.
            feat.process_properties(&mut collector)
                .map_err(|e| DecoderError::Parse {
                    format: "flatgeobuf",
                    source: Box::new(e),
                })?;

            let cont = sink(DecodedFeature {
                // fgb has no native fid; use 0 so the stream layer assigns
                // the row index. (effective_id == row_idx when feature_id == 0)
                feature_id: 0,
                geometry_wkb: Bytes::from(wkb),
                attributes: collector.into_map(),
            });
            row_idx = row_idx.saturating_add(1);
            if !cont {
                break;
            }
        }
        let _ = row_idx;
        Ok(())
    }
}

fn parse_err(e: flatgeobuf::Error) -> DecoderError {
    DecoderError::Parse {
        format: "flatgeobuf",
        source: Box::new(e),
    }
}

struct PropertyCollector {
    out: HashMap<String, AttrValue>,
}

impl PropertyCollector {
    fn new() -> Self {
        Self { out: HashMap::new() }
    }

    fn into_map(self) -> HashMap<String, AttrValue> {
        self.out
    }
}

impl PropertyProcessor for PropertyCollector {
    fn property(&mut self, _idx: usize, name: &str, value: &geozero::ColumnValue<'_>) -> geozero::error::Result<bool> {
        let v = match *value {
            geozero::ColumnValue::Bool(b) => AttrValue::Bool(b),
            geozero::ColumnValue::Byte(b) => AttrValue::Int(i64::from(b)),
            geozero::ColumnValue::UByte(b) => AttrValue::Int(i64::from(b)),
            geozero::ColumnValue::Short(s) => AttrValue::Int(i64::from(s)),
            geozero::ColumnValue::UShort(s) => AttrValue::Int(i64::from(s)),
            geozero::ColumnValue::Int(i) => AttrValue::Int(i64::from(i)),
            geozero::ColumnValue::UInt(i) => AttrValue::Int(i64::from(i)),
            geozero::ColumnValue::Long(i) => AttrValue::Int(i),
            geozero::ColumnValue::ULong(i) => {
                // saturating cast keeps the surface api integer; truly large
                // u64 ids should arrive on the geometry side, not as attrs.
                let v: i64 = i64::try_from(i).unwrap_or(i64::MAX);
                AttrValue::Int(v)
            }
            geozero::ColumnValue::Float(f) => AttrValue::Float(f64::from(f)),
            geozero::ColumnValue::Double(f) => AttrValue::Float(f),
            geozero::ColumnValue::String(s) => AttrValue::String(s.to_string()),
            geozero::ColumnValue::Json(s) => AttrValue::String(s.to_string()),
            geozero::ColumnValue::DateTime(s) => AttrValue::String(s.to_string()),
            geozero::ColumnValue::Binary(_) => AttrValue::Null,
        };
        self.out.insert(name.to_string(), v);
        Ok(false)
    }
}

// FeatureProcessor / GeomProcessor are required as supertraits in some
// geozero paths; the property-only collector here doesn't need them, so
// we provide minimal default impls via `impl FeatureProcessor / GeomProcessor`
// where the trait bound bites. PropertyProcessor stands alone for
// `feat.process_properties`.

// The geozero docs note process_properties only invokes PropertyProcessor,
// so no Feature/Geom impls are needed for this collector.

#[cfg(test)]
mod tests;
