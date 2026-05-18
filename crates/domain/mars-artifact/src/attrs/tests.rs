#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use proptest::prelude::*;

#[test]
fn roundtrip_all_variants() {
    let row = vec![
        ("n".into(), AttrValue::Null),
        ("b".into(), AttrValue::Bool(true)),
        ("i".into(), AttrValue::Int(-42)),
        ("f".into(), AttrValue::Float(2.5)),
        ("s".into(), AttrValue::String("hello".into())),
    ];
    let bytes = encode_row(&row).unwrap();
    let back = decode_row(&bytes).unwrap();
    assert_eq!(back, row);
}

#[test]
fn empty_row_roundtrips() {
    let bytes = encode_row(&[]).unwrap();
    assert_eq!(decode_row(&bytes).unwrap(), Vec::new());
}

#[test]
fn huge_row_count_in_header_rejected() {
    // declare u32::MAX entries in a tiny buffer; must not allocate
    let mut buf = Vec::new();
    buf.extend_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(decode_row(&buf), Err(AttrError::UnexpectedEof)));
}

#[test]
fn oversize_block_rejected() {
    let big = vec![0u8; MAX_ROW_BYTES + 1];
    assert!(matches!(decode_row(&big), Err(AttrError::TooLarge { .. })));
}

#[test]
fn unknown_tag_rejected() {
    // 1 entry, name "x", tag=99
    let mut buf = Vec::new();
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.push(b'x');
    buf.push(99);
    assert!(matches!(decode_row(&buf), Err(AttrError::UnknownTag(99))));
}

#[test]
fn truncated_input_rejected() {
    let row = vec![("k".into(), AttrValue::Int(1))];
    let bytes = encode_row(&row).unwrap();
    let truncated = &bytes[..bytes.len() - 1];
    assert!(matches!(decode_row(truncated), Err(AttrError::UnexpectedEof)));
}

fn arb_attr() -> impl Strategy<Value = AttrValue> {
    prop_oneof![
        Just(AttrValue::Null),
        any::<bool>().prop_map(AttrValue::Bool),
        any::<i64>().prop_map(AttrValue::Int),
        any::<f64>()
            .prop_filter("finite", |f| f.is_finite())
            .prop_map(AttrValue::Float),
        ".{0,32}".prop_map(AttrValue::String),
    ]
}

proptest! {
    #[test]
    fn roundtrip_random(rows in proptest::collection::vec(("[a-z]{1,8}".prop_map(String::from), arb_attr()), 0..16)) {
        let bytes = encode_row(&rows).unwrap();
        prop_assume!(bytes.len() <= MAX_ROW_BYTES);
        let back = decode_row(&bytes).unwrap();
        prop_assert_eq!(back, rows);
    }
}

fn encoded(values: &[(&str, AttrValue)]) -> Vec<u8> {
    let owned: Vec<(String, AttrValue)> = values.iter().map(|(k, v)| ((*k).into(), v.clone())).collect();
    encode_row(&owned).unwrap().to_vec()
}

#[test]
fn section_roundtrip_small() {
    let r1 = encoded(&[("name", AttrValue::String("a".into())), ("k", AttrValue::Int(1))]);
    let r2 = encoded(&[("name", AttrValue::String("b".into())), ("k", AttrValue::Int(2))]);
    let r3 = encoded(&[("name", AttrValue::String("c".into())), ("k", AttrValue::Int(3))]);
    let bytes = encode_attributes_section(&[(7, &r1), (3, &r2), (42, &r3)]).unwrap();
    let sec = AttributesSection::open(&bytes).unwrap();
    assert_eq!(sec.len(), 3);

    let got = sec.lookup(3).unwrap().unwrap();
    assert_eq!(decode_row(got).unwrap(), decode_row(&r2).unwrap());
    let got = sec.lookup(7).unwrap().unwrap();
    assert_eq!(decode_row(got).unwrap(), decode_row(&r1).unwrap());
    let got = sec.lookup(42).unwrap().unwrap();
    assert_eq!(decode_row(got).unwrap(), decode_row(&r3).unwrap());

    assert!(sec.lookup(0).unwrap().is_none());
    assert!(sec.lookup(99).unwrap().is_none());
}

#[test]
fn section_roundtrip_1k() {
    let mut rows: Vec<(u32, Vec<u8>)> = (0..1000)
        .map(|i| {
            let payload = encoded(&[("k", AttrValue::Int(i as i64))]);
            (i as u32 * 31 + 5, payload)
        })
        .collect();
    let refs: Vec<(u32, &[u8])> = rows.iter().map(|(idx, p)| (*idx, p.as_slice())).collect();
    let bytes = encode_attributes_section(&refs).unwrap();
    let sec = AttributesSection::open(&bytes).unwrap();

    // sample 100 entries and confirm decoded rows match.
    for i in (0..1000).step_by(10) {
        let idx = i as u32 * 31 + 5;
        let got = sec.lookup(idx).unwrap().unwrap();
        let expected = &rows[i].1;
        assert_eq!(got, expected.as_slice(), "row {idx}");
    }
    // a missing slot between two present ones falls through.
    assert!(sec.lookup(rows[0].0 + 1).unwrap().is_none());
    rows.clear();
}

#[test]
fn section_empty() {
    let bytes = encode_attributes_section(&[]).unwrap();
    let sec = AttributesSection::open(&bytes).unwrap();
    assert!(sec.is_empty());
    assert!(sec.lookup(0).unwrap().is_none());
    assert!(sec.lookup(u32::MAX).unwrap().is_none());
}

#[test]
fn section_rejects_duplicate_slot_at_encode() {
    let r = encoded(&[("k", AttrValue::Int(1))]);
    let err = encode_attributes_section(&[(5, &r), (5, &r)]).unwrap_err();
    assert!(matches!(err, AttrError::SectionDuplicateFeatureIdx(5)));
}

#[test]
fn section_rejects_truncated_buffer() {
    let r = encoded(&[("k", AttrValue::Int(1))]);
    let bytes = encode_attributes_section(&[(1, &r)]).unwrap();
    for cut in 0..bytes.len() {
        let truncated = &bytes[..cut];
        assert!(AttributesSection::open(truncated).is_err(), "should reject cut={cut}");
    }
}

#[test]
fn section_rejects_bad_magic() {
    let r = encoded(&[("k", AttrValue::Int(1))]);
    let bytes = encode_attributes_section(&[(1, &r)]).unwrap();
    let mut munged = bytes.to_vec();
    munged[0] ^= 0xff;
    assert!(matches!(
        AttributesSection::open(&munged),
        Err(AttrError::SectionBadHeader)
    ));
}

#[test]
fn section_rejects_unsorted_directory() {
    // hand-craft a section with a directory that is sorted descending.
    let r = encoded(&[("k", AttrValue::Int(1))]);
    let mut buf = Vec::new();
    buf.extend_from_slice(SECTION_MAGIC);
    buf.extend_from_slice(&SECTION_VERSION.to_le_bytes());
    buf.extend_from_slice(&2u32.to_le_bytes()); // count
    buf.extend_from_slice(&0u32.to_le_bytes()); // dir_offset placeholder

    let off1 = buf.len() as u32;
    buf.extend_from_slice(&(r.len() as u32).to_le_bytes());
    buf.extend_from_slice(&r);
    let off2 = buf.len() as u32;
    buf.extend_from_slice(&(r.len() as u32).to_le_bytes());
    buf.extend_from_slice(&r);

    let dir_off = buf.len() as u32;
    // descending: 9 then 1.
    buf.extend_from_slice(&9u32.to_le_bytes());
    buf.extend_from_slice(&off1.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&off2.to_le_bytes());

    // patch dir_offset.
    buf[16..20].copy_from_slice(&dir_off.to_le_bytes());

    assert!(matches!(AttributesSection::open(&buf), Err(AttrError::SectionUnsorted)));
}
