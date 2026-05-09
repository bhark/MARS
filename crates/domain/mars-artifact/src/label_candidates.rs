//! label_candidates section codec.
//!
//! Wire format (little-endian throughout):
//!
//! ```text
//! u32 count
//! repeat count times:
//!   u8  flags        bit0 = foreign_origin
//!                    bit1 = has_slot (when set, a u32 feature_idx follows
//!                                     the style/priority pair; when clear
//!                                     the candidate is a pruned-feature
//!                                     label whose geometry was filtered
//!                                     out at this level and so has no
//!                                     per-page slot)
//!                    bit2..3 = shape (0=Point, 1=Polyline, 2=PolygonAnchor)
//!   u16 priority
//!   u16 style_ref_idx
//!   if flags & HAS_SLOT:
//!     u32 feature_idx
//!   if shape == Point or PolygonAnchor:
//!     f32 anchor_x, anchor_y
//!   if shape == Polyline:
//!     u16 vertex_count
//!     vertex_count * (f32, f32)
//!   u16 text_len
//!   text_len bytes utf-8
//! ```
//!
//! Slot-bearing candidates appear in feature_idx ascending order; equal slots
//! are permitted (a feature may emit multiple candidates - polyline repeats,
//! etc.) but must remain contiguous. Pruned-feature candidates (no slot) are
//! collected at the section tail in stable caller-supplied order so they can
//! coexist with slotted entries without violating the ascending invariant.

use bytes::Bytes;

use crate::ArtifactError;

#[derive(Debug, Clone, PartialEq)]
pub struct LabelCandidate {
    /// Per-page slot of the feature this label belongs to. `None` denotes a
    /// pruned-feature label (geometry filtered out at this level by
    /// `Independent` survival policy).
    pub feature_idx: Option<u32>,
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
const FLAG_HAS_SLOT: u8 = 1 << 1;
const SHAPE_SHIFT: u8 = 2;
const SHAPE_MASK: u8 = 0b11 << SHAPE_SHIFT;

// minimum bytes any candidate occupies on the wire: 1 (flags) + 2 (prio)
// + 2 (style) + 2 (text_len) = 7. point/polygon shapes add 8; an empty
// polyline adds 2. slot-bearing entries add 4 more. 7 is the cheapest legal
// entry, used to bound the count up front.
const MIN_ENTRY_LEN: usize = 1 + 2 + 2 + 2;

// hard limits on per-candidate sizes. wire encodes each as u16; cap matches the
// representable range and keeps the decoder bounded. mirrors MAX_GEOM_COORDS in
// spirit: encoder must reject input it cannot faithfully serialise.
pub(crate) const MAX_LABEL_VERTS: usize = u16::MAX as usize;
pub(crate) const MAX_LABEL_TEXT_BYTES: usize = u16::MAX as usize;

/// encoder mirrors decoder: slot-bearing candidates must be ascending by
/// feature_idx (equal allowed for repeats); slotless entries are emitted in
/// caller-supplied order. validates input rather than emit a blob the
/// decoder will reject.
pub fn encode_label_candidates(items: &[LabelCandidate]) -> Result<Bytes, ArtifactError> {
    let mut prev_idx: Option<u32> = None;
    let mut seen_slotless = false;
    for c in items {
        match c.feature_idx {
            Some(idx) => {
                if seen_slotless {
                    return Err(ArtifactError::Malformed(
                        "slot-bearing label candidate after a slotless one",
                    ));
                }
                if let Some(p) = prev_idx
                    && idx < p
                {
                    return Err(ArtifactError::Malformed(
                        "label candidates must be ascending by feature_idx",
                    ));
                }
                prev_idx = Some(idx);
            }
            None => {
                seen_slotless = true;
            }
        }
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
        let shape_bits = match c.shape {
            LabelShape::Point { .. } => SHAPE_POINT,
            LabelShape::Polyline(_) => SHAPE_POLYLINE,
            LabelShape::PolygonAnchor { .. } => SHAPE_POLYGON_ANCHOR,
        };
        let mut flags = (shape_bits << SHAPE_SHIFT) & SHAPE_MASK;
        if c.foreign_origin {
            flags |= FLAG_FOREIGN;
        }
        if c.feature_idx.is_some() {
            flags |= FLAG_HAS_SLOT;
        }
        out.push(flags);
        out.extend_from_slice(&c.priority.to_le_bytes());
        out.extend_from_slice(&c.style_ref_idx.to_le_bytes());
        if let Some(idx) = c.feature_idx {
            out.extend_from_slice(&idx.to_le_bytes());
        }
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
    let mut prev_idx: Option<u32> = None;
    let mut seen_slotless = false;
    for _ in 0..n {
        if bytes.len() < pos + 1 + 2 + 2 {
            return Err(ArtifactError::Truncated);
        }
        let flags = bytes[pos];
        pos += 1;
        let priority = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]);
        pos += 2;
        let style_ref_idx = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]);
        pos += 2;

        let feature_idx = if flags & FLAG_HAS_SLOT != 0 {
            if bytes.len() < pos + 4 {
                return Err(ArtifactError::Truncated);
            }
            let idx = u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]);
            pos += 4;
            if seen_slotless {
                return Err(ArtifactError::Malformed(
                    "slot-bearing label candidate after a slotless one",
                ));
            }
            if let Some(p) = prev_idx
                && idx < p
            {
                return Err(ArtifactError::Malformed(
                    "label candidates must be ascending by feature_idx",
                ));
            }
            prev_idx = Some(idx);
            Some(idx)
        } else {
            seen_slotless = true;
            None
        };

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
            feature_idx,
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
                feature_idx: Some(1),
                foreign_origin: false,
                priority: 100,
                style_ref_idx: 0,
                shape: LabelShape::Point { x: 1.5, y: -2.25 },
                text: "alpha".into(),
            },
            LabelCandidate {
                feature_idx: Some(2),
                foreign_origin: true,
                priority: 50,
                style_ref_idx: 3,
                shape: LabelShape::Polyline(vec![(0.0, 0.0), (10.0, 0.0), (10.0, 5.0)]),
                text: "Ø greek δ".into(),
            },
            LabelCandidate {
                feature_idx: Some(3),
                foreign_origin: false,
                priority: 0,
                style_ref_idx: 7,
                shape: LabelShape::PolygonAnchor { x: 100.0, y: 200.0 },
                text: String::new(),
            },
            // pruned-feature label: no slot.
            LabelCandidate {
                feature_idx: None,
                foreign_origin: false,
                priority: 25,
                style_ref_idx: 0,
                shape: LabelShape::Point { x: 9.0, y: 9.0 },
                text: "pruned".into(),
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
                feature_idx: Some(5),
                foreign_origin: false,
                priority: 0,
                style_ref_idx: 0,
                shape: LabelShape::Point { x: 0.0, y: 0.0 },
                text: "a".into(),
            },
            LabelCandidate {
                feature_idx: Some(1),
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
    fn encoder_rejects_slotted_after_slotless() {
        let bad = vec![
            LabelCandidate {
                feature_idx: None,
                foreign_origin: false,
                priority: 0,
                style_ref_idx: 0,
                shape: LabelShape::Point { x: 0.0, y: 0.0 },
                text: "p".into(),
            },
            LabelCandidate {
                feature_idx: Some(1),
                foreign_origin: false,
                priority: 0,
                style_ref_idx: 0,
                shape: LabelShape::Point { x: 0.0, y: 0.0 },
                text: "s".into(),
            },
        ];
        assert!(matches!(
            encode_label_candidates(&bad),
            Err(ArtifactError::Malformed(_))
        ));
    }

    #[test]
    fn rejects_oversized_text() {
        let big = LabelCandidate {
            feature_idx: Some(1),
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
            feature_idx: Some(1),
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
        // hand-craft: count=1, flags=HAS_SLOT|Point, prio=0, idx=0, slot=0,
        // x=0, y=0, text_len=2, two invalid utf-8 bytes
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.push(FLAG_HAS_SLOT);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes()); // feature_idx
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
