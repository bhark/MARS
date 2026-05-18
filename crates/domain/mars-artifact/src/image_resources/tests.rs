#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn sample() -> Vec<ImageResource> {
    vec![
        ImageResource {
            name: "brick".into(),
            bytes: Bytes::from_static(b"\x89PNG\x0d\x0a\x1a\x0afake"),
        },
        ImageResource {
            name: "grass".into(),
            bytes: Bytes::from_static(b"jpeg-ish-bytes"),
        },
        ImageResource {
            name: "stone".into(),
            bytes: Bytes::from_static(b"x"),
        },
    ]
}

#[test]
fn round_trip() {
    let xs = sample();
    let bytes = encode_image_resources(&xs).unwrap();
    let back = decode_image_resources(&bytes).unwrap();
    assert_eq!(xs, back);
}

#[test]
fn empty_round_trip() {
    let bytes = encode_image_resources(&[]).unwrap();
    assert_eq!(decode_image_resources(&bytes).unwrap(), Vec::<ImageResource>::new());
}

#[test]
fn encoder_rejects_duplicate_name() {
    let xs = vec![
        ImageResource {
            name: "brick".into(),
            bytes: Bytes::from_static(b"a"),
        },
        ImageResource {
            name: "brick".into(),
            bytes: Bytes::from_static(b"b"),
        },
    ];
    assert!(matches!(encode_image_resources(&xs), Err(ArtifactError::Malformed(_))));
}

#[test]
fn encoder_rejects_unsorted() {
    let xs = vec![
        ImageResource {
            name: "stone".into(),
            bytes: Bytes::from_static(b"a"),
        },
        ImageResource {
            name: "brick".into(),
            bytes: Bytes::from_static(b"b"),
        },
    ];
    assert!(matches!(encode_image_resources(&xs), Err(ArtifactError::Malformed(_))));
}

#[test]
fn encoder_rejects_empty_name() {
    let xs = vec![ImageResource {
        name: String::new(),
        bytes: Bytes::from_static(b"a"),
    }];
    assert!(matches!(encode_image_resources(&xs), Err(ArtifactError::Malformed(_))));
}

#[test]
fn encoder_rejects_empty_payload() {
    let xs = vec![ImageResource {
        name: "brick".into(),
        bytes: Bytes::new(),
    }];
    assert!(matches!(encode_image_resources(&xs), Err(ArtifactError::Malformed(_))));
}

#[test]
fn decoder_rejects_truncated_count() {
    assert!(matches!(
        decode_image_resources(&[0x00, 0x00]),
        Err(ArtifactError::Truncated)
    ));
}

#[test]
fn decoder_rejects_oversized_count() {
    let mut bytes = vec![];
    bytes.extend_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(decode_image_resources(&bytes), Err(ArtifactError::Truncated)));
}

#[test]
fn decoder_rejects_trailing_bytes() {
    let xs = sample();
    let mut bytes = encode_image_resources(&xs).unwrap().to_vec();
    bytes.push(0xff);
    assert!(matches!(
        decode_image_resources(&bytes),
        Err(ArtifactError::Malformed(_))
    ));
}

#[test]
fn decoder_rejects_bad_name_utf8() {
    // hand-craft: count=1, name_len=2, name=invalid utf8, image_len=1, image=0
    let mut bytes = vec![];
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&[0xff, 0xfe]);
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.push(0x00);
    assert!(matches!(
        decode_image_resources(&bytes),
        Err(ArtifactError::Malformed(_))
    ));
}
