//! label_candidates section codec.
//!
//! Wire format (little-endian throughout):
//!
//! ```text
//! u32 count
//! repeat count times:
//!   u64 feature_id
//!   u8  flags        bit0 = foreign_origin
//!                    bit1..2 = shape (0=Point, 1=Polyline, 2=PolygonAnchor)
//!   u16 priority
//!   u16 style_ref_idx
//!   if shape == Point or PolygonAnchor:
//!     f32 anchor_x, anchor_y
//!   if shape == Polyline:
//!     u16 vertex_count
//!     vertex_count * (f32, f32)
//!   u16 text_len
//!   text_len bytes utf-8
//! ```
//!
//! Candidates appear in feature-id ascending order, mirroring class_assignment;
//! a single feature may emit multiple candidates (one polyline, repeats, etc.)
//! so equal feature_ids are permitted but must remain contiguous.

use bytes::Bytes;

use crate::ArtifactError;

#[derive(Debug, Clone, PartialEq)]
pub struct LabelCandidate {
    pub feature_id: u64,
    pub foreign_origin: bool,
    pub priority: u16,
    pub style_ref_idx: u16,
    pub shape: LabelShape,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LabelShape {
    Point { x: f32, y: f32 },
    Polyline(Vec<(f32, f32)>),
    PolygonAnchor { x: f32, y: f32 },
}

const SHAPE_POINT: u8 = 0;
const SHAPE_POLYLINE: u8 = 1;
const SHAPE_POLYGON_ANCHOR: u8 = 2;

const FLAG_FOREIGN: u8 = 1 << 0;
const SHAPE_SHIFT: u8 = 1;
const SHAPE_MASK: u8 = 0b11 << SHAPE_SHIFT;

// minimum bytes any candidate occupies on the wire: 8 (id) + 1 (flags) + 2 (prio)
// + 2 (style) + 2 (text_len) = 15. point/polygon shapes add 8; an empty polyline
// adds 2. 15 is the cheapest legal entry, used to bound the count up front.
const MIN_ENTRY_LEN: usize = 8 + 1 + 2 + 2 + 2;

// hard limits on per-candidate sizes. wire encodes each as u16; cap matches the
// representable range and keeps the decoder bounded. mirrors MAX_GEOM_COORDS in
// spirit: encoder must reject input it cannot faithfully serialise.
pub(crate) const MAX_LABEL_VERTS: usize = u16::MAX as usize;
pub(crate) const MAX_LABEL_TEXT_BYTES: usize = u16::MAX as usize;

/// encoder mirrors decoder: feature_id ascending (equal allowed for repeats).
/// validates input rather than emit a blob the decoder will reject.
pub fn encode_label_candidates(items: &[LabelCandidate]) -> Result<Bytes, ArtifactError> {
    let mut prev: Option<u64> = None;
    for c in items {
        if let Some(p) = prev
            && c.feature_id < p
        {
            return Err(ArtifactError::Malformed(
                "label candidates must be ascending by feature_id",
            ));
        }
        prev = Some(c.feature_id);
        if c.text.len() > MAX_LABEL_TEXT_BYTES {
            return Err(ArtifactError::Malformed("label text exceeds max bytes"));
        }
        if let LabelShape::Polyline(verts) = &c.shape
            && verts.len() > MAX_LABEL_VERTS
        {
            return Err(ArtifactError::Malformed("label polyline exceeds max vertices"));
        }
    }
    let mut out = Vec::with_capacity(4 + items.len() * (MIN_ENTRY_LEN + 8));
    out.extend_from_slice(&(items.len() as u32).to_le_bytes());
    for c in items {
        out.extend_from_slice(&c.feature_id.to_le_bytes());
        let shape_bits = match c.shape {
            LabelShape::Point { .. } => SHAPE_POINT,
            LabelShape::Polyline(_) => SHAPE_POLYLINE,
            LabelShape::PolygonAnchor { .. } => SHAPE_POLYGON_ANCHOR,
        };
        let mut flags = (shape_bits << SHAPE_SHIFT) & SHAPE_MASK;
        if c.foreign_origin {
            flags |= FLAG_FOREIGN;
        }
        out.push(flags);
        out.extend_from_slice(&c.priority.to_le_bytes());
        out.extend_from_slice(&c.style_ref_idx.to_le_bytes());
        match &c.shape {
            LabelShape::Point { x, y } | LabelShape::PolygonAnchor { x, y } => {
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
            }
            LabelShape::Polyline(verts) => {
                let vc = u16::try_from(verts.len())
                    .map_err(|_| ArtifactError::Malformed("label polyline exceeds max vertices"))?;
                out.extend_from_slice(&vc.to_le_bytes());
                for (x, y) in verts {
                    out.extend_from_slice(&x.to_le_bytes());
                    out.extend_from_slice(&y.to_le_bytes());
                }
            }
        }
        let bytes = c.text.as_bytes();
        let tlen = u16::try_from(bytes.len()).map_err(|_| ArtifactError::Malformed("label text exceeds max bytes"))?;
        out.extend_from_slice(&tlen.to_le_bytes());
        out.extend_from_slice(bytes);
    }
    Ok(Bytes::from(out))
}

pub fn decode_label_candidates(bytes: &[u8]) -> Result<Vec<LabelCandidate>, ArtifactError> {
    if bytes.len() < 4 {
        return Err(ArtifactError::Truncated);
    }
    let n = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let max_possible = bytes.len().saturating_sub(4) / MIN_ENTRY_LEN;
    if n > max_possible {
        return Err(ArtifactError::Truncated);
    }
    let mut pos = 4;
    let mut out = Vec::with_capacity(n);
    let mut prev_id: Option<u64> = None;
    for _ in 0..n {
        if bytes.len() < pos + 8 + 1 + 2 + 2 {
            return Err(ArtifactError::Truncated);
        }
        let feature_id = u64::from_le_bytes(bytes[pos..pos + 8].try_into().map_err(|_| ArtifactError::Truncated)?);
        pos += 8;
        let flags = bytes[pos];
        pos += 1;
        let priority = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]);
        pos += 2;
        let style_ref_idx = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]);
        pos += 2;

        if let Some(p) = prev_id
            && feature_id < p
        {
            return Err(ArtifactError::Malformed(
                "label candidates must be ascending by feature_id",
            ));
        }
        prev_id = Some(feature_id);

        let foreign_origin = flags & FLAG_FOREIGN != 0;
        let shape_bits = (flags & SHAPE_MASK) >> SHAPE_SHIFT;
        let shape = match shape_bits {
            SHAPE_POINT | SHAPE_POLYGON_ANCHOR => {
                if bytes.len() < pos + 8 {
                    return Err(ArtifactError::Truncated);
                }
                let x = f32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]);
                let y = f32::from_le_bytes([bytes[pos + 4], bytes[pos + 5], bytes[pos + 6], bytes[pos + 7]]);
                pos += 8;
                if shape_bits == SHAPE_POINT {
                    LabelShape::Point { x, y }
                } else {
                    LabelShape::PolygonAnchor { x, y }
                }
            }
            SHAPE_POLYLINE => {
                if bytes.len() < pos + 2 {
                    return Err(ArtifactError::Truncated);
                }
                let vc = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize;
                pos += 2;
                let need = vc.checked_mul(8).ok_or(ArtifactError::Truncated)?;
                if bytes.len() < pos + need {
                    return Err(ArtifactError::Truncated);
                }
                let mut verts = Vec::with_capacity(vc);
                for _ in 0..vc {
                    let x = f32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]);
                    let y = f32::from_le_bytes([bytes[pos + 4], bytes[pos + 5], bytes[pos + 6], bytes[pos + 7]]);
                    verts.push((x, y));
                    pos += 8;
                }
                LabelShape::Polyline(verts)
            }
            _ => return Err(ArtifactError::Malformed("unknown label shape")),
        };

        if bytes.len() < pos + 2 {
            return Err(ArtifactError::Truncated);
        }
        let tlen = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        pos += 2;
        if bytes.len() < pos + tlen {
            return Err(ArtifactError::Truncated);
        }
        let text = std::str::from_utf8(&bytes[pos..pos + tlen])
            .map_err(|_| ArtifactError::Malformed("label text utf8"))?
            .to_owned();
        pos += tlen;

        out.push(LabelCandidate {
            feature_id,
            foreign_origin,
            priority,
            style_ref_idx,
            shape,
            text,
        });
    }
    if pos != bytes.len() {
        return Err(ArtifactError::Malformed("label_candidates trailing bytes"));
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn sample() -> Vec<LabelCandidate> {
        vec![
            LabelCandidate {
                feature_id: 1,
                foreign_origin: false,
                priority: 100,
                style_ref_idx: 0,
                shape: LabelShape::Point { x: 1.5, y: -2.25 },
                text: "alpha".into(),
            },
            LabelCandidate {
                feature_id: 2,
                foreign_origin: true,
                priority: 50,
                style_ref_idx: 3,
                shape: LabelShape::Polyline(vec![(0.0, 0.0), (10.0, 0.0), (10.0, 5.0)]),
                text: "Ø greek δ".into(),
            },
            LabelCandidate {
                feature_id: 3,
                foreign_origin: false,
                priority: 0,
                style_ref_idx: 7,
                shape: LabelShape::PolygonAnchor { x: 100.0, y: 200.0 },
                text: String::new(),
            },
        ]
    }

    #[test]
    fn round_trip() {
        let cs = sample();
        let bytes = encode_label_candidates(&cs).unwrap();
        let decoded = decode_label_candidates(&bytes).unwrap();
        assert_eq!(cs, decoded);
    }

    #[test]
    fn empty_round_trip() {
        let bytes = encode_label_candidates(&[]).unwrap();
        assert_eq!(decode_label_candidates(&bytes).unwrap(), Vec::<LabelCandidate>::new());
    }

    #[test]
    fn rejects_truncated_count() {
        assert!(matches!(
            decode_label_candidates(&[0x00, 0x00]),
            Err(ArtifactError::Truncated)
        ));
    }

    #[test]
    fn rejects_oversized_count() {
        let mut bytes = vec![];
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(decode_label_candidates(&bytes), Err(ArtifactError::Truncated)));
    }

    #[test]
    fn rejects_truncated_body() {
        let cs = sample();
        let bytes = encode_label_candidates(&cs).unwrap();
        // chop off the last byte
        let short = &bytes[..bytes.len() - 1];
        assert!(decode_label_candidates(short).is_err());
    }

    #[test]
    fn encoder_rejects_unsorted_features() {
        let unsorted = vec![
            LabelCandidate {
                feature_id: 5,
                foreign_origin: false,
                priority: 0,
                style_ref_idx: 0,
                shape: LabelShape::Point { x: 0.0, y: 0.0 },
                text: "a".into(),
            },
            LabelCandidate {
                feature_id: 1,
                foreign_origin: false,
                priority: 0,
                style_ref_idx: 0,
                shape: LabelShape::Point { x: 0.0, y: 0.0 },
                text: "b".into(),
            },
        ];
        assert!(matches!(
            encode_label_candidates(&unsorted),
            Err(ArtifactError::Malformed(_))
        ));
    }

    #[test]
    fn rejects_oversized_text() {
        let big = LabelCandidate {
            feature_id: 1,
            foreign_origin: false,
            priority: 0,
            style_ref_idx: 0,
            shape: LabelShape::Point { x: 0.0, y: 0.0 },
            text: "x".repeat(MAX_LABEL_TEXT_BYTES + 1),
        };
        assert!(matches!(
            encode_label_candidates(&[big]),
            Err(ArtifactError::Malformed(_))
        ));
    }

    #[test]
    fn rejects_oversized_polyline() {
        let big = LabelCandidate {
            feature_id: 1,
            foreign_origin: false,
            priority: 0,
            style_ref_idx: 0,
            shape: LabelShape::Polyline(vec![(0.0, 0.0); MAX_LABEL_VERTS + 1]),
            text: "a".into(),
        };
        assert!(matches!(
            encode_label_candidates(&[big]),
            Err(ArtifactError::Malformed(_))
        ));
    }

    #[test]
    fn rejects_bad_text_utf8() {
        // hand-craft: count=1, fid=0, flags=0 (Point), prio=0, idx=0, x=0, y=0,
        // text_len=2, two invalid utf-8 bytes
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.push(0u8);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0f32.to_le_bytes());
        bytes.extend_from_slice(&0f32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&[0xff, 0xfe]);
        assert!(matches!(
            decode_label_candidates(&bytes),
            Err(ArtifactError::Malformed(_))
        ));
    }
}
