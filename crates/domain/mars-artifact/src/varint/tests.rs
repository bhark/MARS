#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;

fn roundtrip_uvarint(v: u64) {
    let mut buf = Vec::new();
    write_uvarint(&mut buf, v);
    let mut pos = 0;
    let back = read_uvarint(&buf, &mut pos).unwrap();
    assert_eq!(back, v, "uvarint roundtrip failed for {v}");
    assert_eq!(pos, buf.len(), "trailing bytes after uvarint {v}");
}

fn roundtrip_ivarint(v: i64) {
    let mut buf = Vec::new();
    write_ivarint(&mut buf, v);
    let mut pos = 0;
    let back = read_ivarint(&buf, &mut pos).unwrap();
    assert_eq!(back, v, "ivarint roundtrip failed for {v}");
    assert_eq!(pos, buf.len(), "trailing bytes after ivarint {v}");
}

#[test]
fn uvarint_zero() {
    roundtrip_uvarint(0);
}

#[test]
fn uvarint_small_values() {
    for v in [1, 127, 128, 16383, 16384, 0xFFFF, 0x1FFFF] {
        roundtrip_uvarint(v);
    }
}

#[test]
fn uvarint_max() {
    roundtrip_uvarint(u64::MAX);
}

#[test]
fn ivarint_extremes() {
    roundtrip_ivarint(i64::MAX);
    roundtrip_ivarint(i64::MIN);
    roundtrip_ivarint(0);
    roundtrip_ivarint(-1);
    roundtrip_ivarint(1);
}

#[test]
fn zigzag_identity() {
    for v in [i64::MIN, i64::MAX, -1, 0, 1, -2, 2] {
        assert_eq!(zigzag_decode(zigzag_encode(v)), v, "zigzag identity failed for {v}");
    }
}

#[test]
fn truncated_empty() {
    let mut pos = 0;
    assert!(matches!(read_uvarint(&[], &mut pos), Err(ArtifactError::Truncated)));
}

#[test]
fn truncated_mid_sequence() {
    // encode a large value (needs multiple bytes), then drop the tail
    let mut buf = Vec::new();
    write_uvarint(&mut buf, u64::MAX);
    buf.pop(); // remove last byte
    let mut pos = 0;
    assert!(matches!(read_uvarint(&buf, &mut pos), Err(ArtifactError::Truncated)));
}

#[test]
fn overflow_10th_byte_continuation() {
    // 10 bytes, all with continuation bit set → should overflow
    let buf = vec![0xFF; 10];
    let mut pos = 0;
    assert!(matches!(
        read_uvarint(&buf, &mut pos),
        Err(ArtifactError::Malformed("varint overflow"))
    ));
}

#[test]
fn overflow_10th_byte_payload_too_large() {
    // 9 bytes with continuation + 10th byte without continuation but payload > 1
    let mut buf = vec![0xFF; 9];
    buf.push(0x02); // payload 2 at shift 63 → overflow
    let mut pos = 0;
    assert!(matches!(
        read_uvarint(&buf, &mut pos),
        Err(ArtifactError::Malformed("varint overflow"))
    ));
}

#[test]
fn valid_10th_byte_for_u64_max() {
    // u64::MAX encoded as 10 bytes: 9x 0xFF, last is 0x01
    let mut buf = vec![0xFF; 9];
    buf.push(0x01);
    let mut pos = 0;
    let v = read_uvarint(&buf, &mut pos).unwrap();
    assert_eq!(v, u64::MAX);
}
