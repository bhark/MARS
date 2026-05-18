#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn entries(n: u64) -> Vec<(u64, HilbertKey)> {
    (0..n)
        .map(|i| (i * 31 + 7, HilbertKey::new(i.wrapping_mul(0x9E37_79B9_7F4A_7C15))))
        .collect()
}

#[test]
fn roundtrip_small() {
    let mut e = vec![
        (3u64, HilbertKey::new(30)),
        (1, HilbertKey::new(10)),
        (2, HilbertKey::new(20)),
    ];
    let bytes = encode_sidecar(&mut e).unwrap();
    let reader = SidecarReader::open(&bytes).unwrap();
    assert_eq!(reader.len(), 3);
    assert_eq!(reader.lookup_all(1).collect::<Vec<_>>(), vec![HilbertKey::new(10)]);
    assert_eq!(reader.lookup_all(2).collect::<Vec<_>>(), vec![HilbertKey::new(20)]);
    assert_eq!(reader.lookup_all(3).collect::<Vec<_>>(), vec![HilbertKey::new(30)]);
    assert!(reader.lookup_all(0).next().is_none());
    assert!(reader.lookup_all(99).next().is_none());
}

#[test]
fn lookup_all_returns_every_key_for_repeated_user_id() {
    let mut e = vec![
        (5u64, HilbertKey::new(50)),
        (5u64, HilbertKey::new(20)),
        (5u64, HilbertKey::new(30)),
        (1u64, HilbertKey::new(1)),
    ];
    let bytes = encode_sidecar(&mut e).unwrap();
    let reader = SidecarReader::open(&bytes).unwrap();
    assert_eq!(
        reader.lookup_all(5).collect::<Vec<_>>(),
        vec![HilbertKey::new(20), HilbertKey::new(30), HilbertKey::new(50)]
    );
    assert_eq!(reader.lookup_all(1).collect::<Vec<_>>(), vec![HilbertKey::new(1)]);
}

#[test]
fn roundtrip_10k_random_lookups() {
    let mut e = entries(10_000);
    let oracle: std::collections::HashMap<u64, HilbertKey> = e.iter().copied().collect();
    let bytes = encode_sidecar(&mut e).unwrap();
    let reader = SidecarReader::open(&bytes).unwrap();
    assert_eq!(reader.len(), 10_000);
    // 100 spot checks via the oracle.
    let mut state = 1u64;
    for _ in 0..100 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let i = (state >> 32) % 10_000;
        let id = i * 31 + 7;
        let got: Vec<_> = reader.lookup_all(id).collect();
        assert_eq!(got, vec![*oracle.get(&id).unwrap()], "mismatch at id {id}");
    }
    // an id known to be absent.
    assert!(reader.lookup_all(8).next().is_none());
}

#[test]
fn empty_sidecar_roundtrips() {
    let mut e: Vec<(u64, HilbertKey)> = vec![];
    let bytes = encode_sidecar(&mut e).unwrap();
    let reader = SidecarReader::open(&bytes).unwrap();
    assert!(reader.is_empty());
    assert!(reader.lookup_all(0).next().is_none());
}

#[test]
fn duplicate_user_ids_are_legal() {
    // multimap semantics: two entries with the same user_id but
    // different hilbert keys must both round-trip.
    let mut e = vec![(5u64, HilbertKey::new(1)), (5, HilbertKey::new(2))];
    let bytes = encode_sidecar(&mut e).unwrap();
    let reader = SidecarReader::open(&bytes).unwrap();
    assert_eq!(
        reader.lookup_all(5).collect::<Vec<_>>(),
        vec![HilbertKey::new(1), HilbertKey::new(2)]
    );
}

#[test]
fn rejects_bad_magic() {
    let mut e = vec![(1u64, HilbertKey::new(2))];
    let bytes = encode_sidecar(&mut e).unwrap();
    let mut munged = bytes.to_vec();
    munged[0] ^= 0xff;
    assert!(matches!(SidecarReader::open(&munged), Err(SidecarError::BadHeader)));
}

#[test]
fn rejects_truncated_buffer() {
    let mut e = vec![
        (1u64, HilbertKey::new(2)),
        (3u64, HilbertKey::new(4)),
        (5u64, HilbertKey::new(6)),
    ];
    let bytes = encode_sidecar(&mut e).unwrap();
    for cut in 0..bytes.len() {
        let truncated = &bytes[..cut];
        assert!(SidecarReader::open(truncated).is_err(), "should reject cut={cut}");
    }
}

#[test]
fn rejects_bad_version() {
    let mut e = vec![(1u64, HilbertKey::new(2))];
    let bytes = encode_sidecar(&mut e).unwrap();
    let mut munged = bytes.to_vec();
    munged[4] = 99;
    assert!(matches!(SidecarReader::open(&munged), Err(SidecarError::BadHeader)));
}

#[test]
fn iter_yields_all_pairs_in_feature_id_order() {
    let mut e = vec![
        (5u64, HilbertKey::new(50)),
        (1u64, HilbertKey::new(10)),
        (3u64, HilbertKey::new(30)),
    ];
    let bytes = encode_sidecar(&mut e).unwrap();
    let reader = SidecarReader::open(&bytes).unwrap();
    let pairs: Vec<(u64, HilbertKey)> = reader.iter().collect();
    assert_eq!(
        pairs,
        vec![
            (1, HilbertKey::new(10)),
            (3, HilbertKey::new(30)),
            (5, HilbertKey::new(50)),
        ]
    );
}

#[test]
fn user_ids_in_ranges_filters_by_key() {
    let mut e = vec![
        (10u64, HilbertKey::new(100)),
        (20u64, HilbertKey::new(200)),
        (30u64, HilbertKey::new(300)),
        (40u64, HilbertKey::new(400)),
        (50u64, HilbertKey::new(500)),
    ];
    let bytes = encode_sidecar(&mut e).unwrap();
    let reader = SidecarReader::open(&bytes).unwrap();
    let ranges = vec![
        (HilbertKey::new(150), HilbertKey::new(250)),
        (HilbertKey::new(350), HilbertKey::new(500)),
    ];
    let ids = reader.user_ids_in_ranges(&ranges);
    assert_eq!(ids, vec![20, 40, 50]);

    // empty range list yields empty result.
    let empty = reader.user_ids_in_ranges(&[]);
    assert!(empty.is_empty());
}

#[test]
fn rejects_unsorted_handcrafted() {
    // build a sidecar by hand with descending feature_ids.
    let mut buf = Vec::new();
    buf.extend_from_slice(&MAGIC.to_le_bytes());
    buf.extend_from_slice(&VERSION.to_le_bytes());
    buf.extend_from_slice(&2u64.to_le_bytes());
    buf.extend_from_slice(&9u64.to_le_bytes());
    buf.extend_from_slice(&90u64.to_le_bytes());
    buf.extend_from_slice(&1u64.to_le_bytes());
    buf.extend_from_slice(&10u64.to_le_bytes());
    assert!(matches!(SidecarReader::open(&buf), Err(SidecarError::Unsorted)));
}
