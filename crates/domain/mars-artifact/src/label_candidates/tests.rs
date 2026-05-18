#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

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
