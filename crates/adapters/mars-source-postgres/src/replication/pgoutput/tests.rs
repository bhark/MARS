#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn build_relation(oid: u32, ns: &str, name: &str, cols: &[(&str, u32)]) -> Vec<u8> {
    let mut v = vec![b'R'];
    v.extend_from_slice(&oid.to_be_bytes());
    v.extend_from_slice(ns.as_bytes());
    v.push(0);
    v.extend_from_slice(name.as_bytes());
    v.push(0);
    v.push(b'd'); // replica identity = default
    v.extend_from_slice(&(cols.len() as u16).to_be_bytes());
    for (cname, oid) in cols {
        v.push(0); // flags
        v.extend_from_slice(cname.as_bytes());
        v.push(0);
        v.extend_from_slice(&oid.to_be_bytes());
        v.extend_from_slice(&(-1_i32).to_be_bytes());
    }
    v
}

fn build_insert(oid: u32, cols: &[ColumnData<'_>]) -> Vec<u8> {
    let mut v = vec![b'I'];
    v.extend_from_slice(&oid.to_be_bytes());
    v.push(b'N');
    v.extend_from_slice(&(cols.len() as u16).to_be_bytes());
    for c in cols {
        match c {
            ColumnData::Null => v.push(b'n'),
            ColumnData::Unchanged => v.push(b'u'),
            ColumnData::Text(b) => {
                v.push(b't');
                v.extend_from_slice(&(b.len() as u32).to_be_bytes());
                v.extend_from_slice(b);
            }
            ColumnData::Binary(b) => {
                v.push(b'b');
                v.extend_from_slice(&(b.len() as u32).to_be_bytes());
                v.extend_from_slice(b);
            }
        }
    }
    v
}

#[test]
fn decode_relation_roundtrip() {
    let buf = build_relation(123, "public", "roads", &[("gid", 23), ("geom", 17_834)]);
    let m = decode(&buf).unwrap();
    match m {
        Message::Relation(r) => {
            assert_eq!(r.oid, 123);
            assert_eq!(r.namespace, "public");
            assert_eq!(r.name, "roads");
            assert_eq!(r.columns.len(), 2);
            assert_eq!(r.columns[0].name, "gid");
            assert_eq!(r.columns[1].name, "geom");
            assert_eq!(r.columns[1].type_oid, 17_834);
        }
        other => panic!("expected Relation, got {other:?}"),
    }
}

#[test]
fn decode_insert_with_binary_geom() {
    let geom_bytes = [1u8, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xF0, 0x3F]; // partial wkb-ish payload
    let buf = build_insert(123, &[ColumnData::Text(b"42"), ColumnData::Binary(&geom_bytes)]);
    let m = decode(&buf).unwrap();
    match m {
        Message::Insert { relation_oid, tuple } => {
            assert_eq!(relation_oid, 123);
            assert_eq!(tuple.columns.len(), 2);
            assert_eq!(tuple.columns[0], ColumnData::Text(b"42"));
            assert_eq!(tuple.columns[1], ColumnData::Binary(&geom_bytes));
        }
        other => panic!("expected Insert, got {other:?}"),
    }
}

#[test]
fn decode_update_with_full_old() {
    let mut v = vec![b'U'];
    v.extend_from_slice(&(99_u32).to_be_bytes());
    v.push(b'O'); // full old tuple follows
    // old tuple: 1 column, text "old"
    v.extend_from_slice(&(1_u16).to_be_bytes());
    v.push(b't');
    v.extend_from_slice(&(3_u32).to_be_bytes());
    v.extend_from_slice(b"old");
    // new tuple: 1 column, text "new"
    v.push(b'N');
    v.extend_from_slice(&(1_u16).to_be_bytes());
    v.push(b't');
    v.extend_from_slice(&(3_u32).to_be_bytes());
    v.extend_from_slice(b"new");
    let m = decode(&v).unwrap();
    match m {
        Message::Update { relation_oid, payload } => {
            assert_eq!(relation_oid, 99);
            assert_eq!(payload.full_old.unwrap().columns[0], ColumnData::Text(b"old"));
            assert!(payload.key_old.is_none());
            assert_eq!(payload.new.columns[0], ColumnData::Text(b"new"));
        }
        other => panic!("expected Update, got {other:?}"),
    }
}

#[test]
fn decode_update_without_old() {
    // bare update: relation_oid then 'N' then tuple
    let mut v = vec![b'U'];
    v.extend_from_slice(&(99_u32).to_be_bytes());
    v.push(b'N');
    v.extend_from_slice(&(1_u16).to_be_bytes());
    v.push(b'n');
    let m = decode(&v).unwrap();
    match m {
        Message::Update { payload, .. } => {
            assert!(payload.full_old.is_none());
            assert!(payload.key_old.is_none());
            assert_eq!(payload.new.columns[0], ColumnData::Null);
        }
        other => panic!("expected Update, got {other:?}"),
    }
}

#[test]
fn decode_delete_full_old() {
    let mut v = vec![b'D'];
    v.extend_from_slice(&(7_u32).to_be_bytes());
    v.push(b'O');
    v.extend_from_slice(&(1_u16).to_be_bytes());
    v.push(b't');
    v.extend_from_slice(&(1_u32).to_be_bytes());
    v.push(b'x');
    let m = decode(&v).unwrap();
    match m {
        Message::Delete {
            payload: DeletePayload::Full(t),
            relation_oid,
        } => {
            assert_eq!(relation_oid, 7);
            assert_eq!(t.columns[0], ColumnData::Text(b"x"));
        }
        other => panic!("expected full Delete, got {other:?}"),
    }
}

#[test]
fn decode_truncate_two_relations() {
    let mut v = vec![b'T'];
    v.extend_from_slice(&(2_u32).to_be_bytes());
    v.push(0); // flags
    v.extend_from_slice(&(11_u32).to_be_bytes());
    v.extend_from_slice(&(22_u32).to_be_bytes());
    let m = decode(&v).unwrap();
    match m {
        Message::Truncate(t) => assert_eq!(t.relation_oids, vec![11, 22]),
        other => panic!("expected Truncate, got {other:?}"),
    }
}

#[test]
fn decode_begin_commit_roundtrip() {
    let mut b = vec![b'B'];
    b.extend_from_slice(&(0x1122_3344_5566_7788_u64).to_be_bytes());
    b.extend_from_slice(&(42_i64).to_be_bytes());
    b.extend_from_slice(&(7_u32).to_be_bytes());
    match decode(&b).unwrap() {
        Message::Begin { final_lsn, xid, .. } => {
            assert_eq!(final_lsn, 0x1122_3344_5566_7788);
            assert_eq!(xid, 7);
        }
        _ => panic!("expected Begin"),
    }

    let mut c = vec![b'C', 0]; // flags
    c.extend_from_slice(&(1_u64).to_be_bytes()); // commit_lsn
    c.extend_from_slice(&(2_u64).to_be_bytes()); // end_lsn
    c.extend_from_slice(&(99_i64).to_be_bytes());
    match decode(&c).unwrap() {
        Message::Commit {
            commit_lsn, end_lsn, ..
        } => {
            assert_eq!(commit_lsn, 1);
            assert_eq!(end_lsn, 2);
        }
        _ => panic!("expected Commit"),
    }
}

#[test]
fn truncated_frame_errors_cleanly() {
    let buf = vec![b'I', 0, 0]; // missing oid bytes
    assert!(matches!(decode(&buf), Err(PgOutputError::Truncated)));
}

#[test]
fn unknown_kind_errors() {
    let buf = vec![b'Z'];
    assert!(matches!(decode(&buf), Err(PgOutputError::UnknownKind(b'Z'))));
}

#[test]
fn type_origin_tolerated_as_unhandled() {
    let buf = vec![b'Y']; // bare 'Y' - parser does not even read the body
    assert!(matches!(decode(&buf), Ok(Message::Unhandled)));
    let buf = vec![b'O'];
    assert!(matches!(decode(&buf), Ok(Message::Unhandled)));
}
