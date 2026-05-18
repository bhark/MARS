#![allow(clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use mars_artifact::{FeatureGeom, GeomKind};
use mars_source::AttrValue;
use mars_types::HilbertKey;

use super::*;
use crate::CompilerError;
use crate::render::KeyedRow;

// in-test slice reader; isolates the codec tests from spill / external_sort.
struct Slice<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Slice<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { bytes: b, pos: 0 }
    }
}

impl ScratchReader for Slice<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], CompilerError> {
        if self.pos + n > self.bytes.len() {
            return Err(CompilerError::InvariantViolation {
                what: "test slice: short read",
            });
        }
        let s = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, CompilerError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, CompilerError> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn u64(&mut self) -> Result<u64, CompilerError> {
        let s = self.take(8)?;
        Ok(u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
    }
    fn i64(&mut self) -> Result<i64, CompilerError> {
        Ok(self.u64()? as i64)
    }
    fn f32(&mut self) -> Result<f32, CompilerError> {
        let s = self.take(4)?;
        Ok(f32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn f64(&mut self) -> Result<f64, CompilerError> {
        let s = self.take(8)?;
        Ok(f64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
    }
}

fn row_with(geom: GeomKind, attrs: Vec<(String, AttrValue)>) -> KeyedRow {
    KeyedRow {
        feature: FeatureGeom {
            user_id: 42,
            bbox: [1.0, 2.0, 3.0, 4.0],
            geom,
        },
        attrs: Arc::new(attrs),
        geom_bytes_estimate: 128,
        key: HilbertKey::new(0xCAFE_BABE_DEAD_BEEF),
        row_fingerprint: 0x1234_5678_9ABC_DEF0,
    }
}

fn rows_eq(a: &KeyedRow, b: &KeyedRow) -> bool {
    a.feature == b.feature
        && a.geom_bytes_estimate == b.geom_bytes_estimate
        && a.key == b.key
        && a.row_fingerprint == b.row_fingerprint
        && a.attrs.as_slice() == b.attrs.as_slice()
}

fn roundtrip(row: KeyedRow) -> KeyedRow {
    let mut buf = Vec::new();
    encode_keyed_row_body(&mut buf, &row);
    let mut r = Slice::new(&buf);
    let out = decode_keyed_row_body(&mut r).unwrap();
    assert_eq!(r.pos, buf.len(), "decoder must consume entire buffer");
    out
}

#[test]
fn point_roundtrip() {
    let r = row_with(GeomKind::Point((1.5, 2.5)), vec![]);
    assert!(rows_eq(&r, &roundtrip(r.clone())));
}

#[test]
fn linestring_roundtrip() {
    let r = row_with(GeomKind::LineString(vec![(0.0, 0.0), (1.0, 1.0), (2.0, 2.0)]), vec![]);
    assert!(rows_eq(&r, &roundtrip(r.clone())));
}

#[test]
fn polygon_empty_rings_roundtrip() {
    let r = row_with(GeomKind::Polygon(vec![]), vec![]);
    assert!(rows_eq(&r, &roundtrip(r.clone())));
}

#[test]
fn polygon_one_ring_roundtrip() {
    let r = row_with(
        GeomKind::Polygon(vec![vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)]]),
        vec![],
    );
    assert!(rows_eq(&r, &roundtrip(r.clone())));
}

#[test]
fn multipoint_roundtrip() {
    let r = row_with(GeomKind::MultiPoint(vec![(1.0, 1.0), (2.0, 2.0)]), vec![]);
    assert!(rows_eq(&r, &roundtrip(r.clone())));
}

#[test]
fn multilinestring_roundtrip() {
    let r = row_with(
        GeomKind::MultiLineString(vec![vec![(0.0, 0.0), (1.0, 1.0)], vec![(2.0, 2.0), (3.0, 3.0)]]),
        vec![],
    );
    assert!(rows_eq(&r, &roundtrip(r.clone())));
}

#[test]
fn multipolygon_roundtrip() {
    let r = row_with(
        GeomKind::MultiPolygon(vec![
            vec![vec![(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (0.0, 0.0)]],
            vec![
                vec![(2.0, 2.0), (3.0, 2.0), (3.0, 3.0), (2.0, 2.0)],
                vec![(2.2, 2.2), (2.8, 2.2), (2.5, 2.8), (2.2, 2.2)],
            ],
        ]),
        vec![],
    );
    assert!(rows_eq(&r, &roundtrip(r.clone())));
}

#[test]
fn attr_variants_roundtrip() {
    let r = row_with(
        GeomKind::Point((0.0, 0.0)),
        vec![
            ("a_null".into(), AttrValue::Null),
            ("a_true".into(), AttrValue::Bool(true)),
            ("a_false".into(), AttrValue::Bool(false)),
            ("a_int_neg".into(), AttrValue::Int(-1_234_567_890)),
            ("a_float".into(), AttrValue::Float(std::f64::consts::PI)),
            ("a_str_empty".into(), AttrValue::String(String::new())),
            ("a_str_utf8".into(), AttrValue::String("héllo, 世界 🌍".into())),
        ],
    );
    assert!(rows_eq(&r, &roundtrip(r.clone())));
}

#[test]
fn bad_geom_tag_errors() {
    let mut buf = Vec::new();
    buf.extend_from_slice(&0u64.to_le_bytes()); // key
    buf.extend_from_slice(&0u64.to_le_bytes()); // user_id
    for _ in 0..4 {
        buf.extend_from_slice(&0f32.to_le_bytes()); // bbox
    }
    buf.push(255); // bogus geom tag
    let mut r = Slice::new(&buf);
    let err = decode_keyed_row_body(&mut r).unwrap_err();
    assert!(
        matches!(err, CompilerError::InvariantViolation { what } if what.contains("geom")),
        "expected geom-tag invariant violation",
    );
}

#[test]
fn bad_attr_tag_errors() {
    let mut buf = Vec::new();
    buf.extend_from_slice(&0u64.to_le_bytes()); // key
    buf.extend_from_slice(&0u64.to_le_bytes()); // user_id
    for _ in 0..4 {
        buf.extend_from_slice(&0f32.to_le_bytes()); // bbox
    }
    buf.push(GT_POINT);
    buf.extend_from_slice(&0f64.to_le_bytes()); // x
    buf.extend_from_slice(&0f64.to_le_bytes()); // y
    buf.extend_from_slice(&1u32.to_le_bytes()); // attr_count = 1
    buf.extend_from_slice(&1u32.to_le_bytes()); // name_len = 1
    buf.push(b'a');
    buf.push(255); // bogus attr tag
    let mut r = Slice::new(&buf);
    let err = decode_keyed_row_body(&mut r).unwrap_err();
    assert!(
        matches!(err, CompilerError::InvariantViolation { what } if what.contains("attr")),
        "expected attr-tag invariant violation",
    );
}

#[test]
fn bad_utf8_in_attr_name_errors() {
    let mut buf = Vec::new();
    buf.extend_from_slice(&0u64.to_le_bytes());
    buf.extend_from_slice(&0u64.to_le_bytes());
    for _ in 0..4 {
        buf.extend_from_slice(&0f32.to_le_bytes());
    }
    buf.push(GT_POINT);
    buf.extend_from_slice(&0f64.to_le_bytes());
    buf.extend_from_slice(&0f64.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes()); // attr_count
    buf.extend_from_slice(&1u32.to_le_bytes()); // name_len
    buf.push(0xFF); // invalid utf-8 lead byte
    let mut r = Slice::new(&buf);
    let err = decode_keyed_row_body(&mut r).unwrap_err();
    assert!(
        matches!(err, CompilerError::InvariantViolation { what } if what.contains("utf8")),
        "expected utf8 invariant violation",
    );
}
