//! pgoutput logical-replication output-plugin frame decoder.
//!
//! Wire format reference: postgres docs §53 "Logical Replication Message
//! Formats". We parse the message kinds the change-feed needs:
//!
//! - `B` Begin
//! - `C` Commit
//! - `R` Relation
//! - `Y` Type   (parsed and discarded; layout-affecting only)
//! - `O` Origin (parsed and discarded)
//! - `I` Insert
//! - `U` Update
//! - `D` Delete
//! - `T` Truncate
//! - (`M` LogicalMessage and `S` StreamStop are tolerated and reported as
//!   `Unhandled` rather than refused, so a streaming-feature mid-stream does
//!   not crash the loop.)
//!
//! Tuple TOAST columns: `n` (NULL), `t` (text), `u` (unchanged-toast),
//! `b` (binary). Binary mode is the one we ask for; text mode is tolerated.

use std::io::{Cursor, Read};

/// Decoder errors. All variants are recoverable at the loop level (the
/// stream returns the error to the consumer); none indicate UB.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PgOutputError {
    #[error("truncated pgoutput frame")]
    Truncated,
    #[error("unknown pgoutput message kind: {0:#x}")]
    UnknownKind(u8),
    #[error("unknown tuple column kind: {0:#x}")]
    UnknownColumnKind(u8),
    #[error("invalid utf-8 in pgoutput string")]
    InvalidUtf8,
    #[error("missing trailing NUL in pgoutput string")]
    MissingNul,
}

/// Tuple column data. `Null` and `Unchanged` carry no payload; `Text` and
/// `Binary` carry raw bytes that the caller decodes per-type. For our
/// purposes only the geometry column matters and we parse it as WKB.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ColumnData<'a> {
    Null,
    Unchanged,
    Text(&'a [u8]),
    Binary(&'a [u8]),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Tuple<'a> {
    pub columns: Vec<ColumnData<'a>>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RelationColumn {
    pub flags: u8,
    pub name: String,
    pub type_oid: u32,
    pub type_modifier: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Relation {
    pub oid: u32,
    pub namespace: String,
    pub name: String,
    pub replica_identity: u8,
    pub columns: Vec<RelationColumn>,
}

/// Update message tuple payload. pgoutput emits an optional `K`/`O` tuple
/// for the OLD row (when REPLICA IDENTITY != FULL we get only the key; with
/// FULL we get the full old row).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UpdatePayload<'a> {
    pub key_old: Option<Tuple<'a>>,
    pub full_old: Option<Tuple<'a>>,
    pub new: Tuple<'a>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DeletePayload<'a> {
    KeyOnly(Tuple<'a>),
    Full(Tuple<'a>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TruncatePayload {
    pub relation_oids: Vec<u32>,
    pub flags: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Message<'a> {
    Begin {
        final_lsn: u64,
        commit_timestamp: i64,
        xid: u32,
    },
    Commit {
        flags: u8,
        commit_lsn: u64,
        end_lsn: u64,
        commit_timestamp: i64,
    },
    Relation(Relation),
    Insert {
        relation_oid: u32,
        tuple: Tuple<'a>,
    },
    Update {
        relation_oid: u32,
        payload: UpdatePayload<'a>,
    },
    Delete {
        relation_oid: u32,
        payload: DeletePayload<'a>,
    },
    Truncate(TruncatePayload),
    /// Type / Origin / streaming-protocol messages we tolerate but do not act on.
    Unhandled,
}

/// Decode one pgoutput frame. The buffer is the body of an `XLogData` payload
/// (the framing layer strips the header byte 'w' and the LSN/clock prefix).
pub(crate) fn decode(buf: &[u8]) -> Result<Message<'_>, PgOutputError> {
    if buf.is_empty() {
        return Err(PgOutputError::Truncated);
    }
    let kind = buf[0];
    let mut p = Parser::new(&buf[1..]);
    match kind {
        b'B' => decode_begin(&mut p),
        b'C' => decode_commit(&mut p),
        b'R' => decode_relation(&mut p),
        b'I' => decode_insert(&mut p),
        b'U' => decode_update(&mut p),
        b'D' => decode_delete(&mut p),
        b'T' => decode_truncate(&mut p),
        // Type / Origin / LogicalMessage / Stream*: parsed by reading their
        // payloads only as far as needed to keep the stream synced. since
        // each pgoutput frame is its own copydata payload, we can simply
        // ignore them - the framing layer never consumes more than one
        // frame's bytes.
        b'Y' | b'O' | b'M' | b'S' | b'E' | b'r' | b'l' | b'c' => Ok(Message::Unhandled),
        other => Err(PgOutputError::UnknownKind(other)),
    }
}

fn decode_begin(p: &mut Parser<'_>) -> Result<Message<'static>, PgOutputError> {
    let final_lsn = p.u64()?;
    let commit_timestamp = p.i64()?;
    let xid = p.u32()?;
    Ok(Message::Begin {
        final_lsn,
        commit_timestamp,
        xid,
    })
}

fn decode_commit(p: &mut Parser<'_>) -> Result<Message<'static>, PgOutputError> {
    let flags = p.u8()?;
    let commit_lsn = p.u64()?;
    let end_lsn = p.u64()?;
    let commit_timestamp = p.i64()?;
    Ok(Message::Commit {
        flags,
        commit_lsn,
        end_lsn,
        commit_timestamp,
    })
}

fn decode_relation<'a>(p: &mut Parser<'a>) -> Result<Message<'a>, PgOutputError> {
    let oid = p.u32()?;
    let namespace = p.cstr()?.to_string();
    let name = p.cstr()?.to_string();
    let replica_identity = p.u8()?;
    let n_cols = p.u16()? as usize;
    let mut columns = Vec::with_capacity(n_cols);
    for _ in 0..n_cols {
        let flags = p.u8()?;
        let cname = p.cstr()?.to_string();
        let type_oid = p.u32()?;
        let type_modifier = p.i32()?;
        columns.push(RelationColumn {
            flags,
            name: cname,
            type_oid,
            type_modifier,
        });
    }
    Ok(Message::Relation(Relation {
        oid,
        namespace,
        name,
        replica_identity,
        columns,
    }))
}

fn decode_tuple<'a>(p: &mut Parser<'a>) -> Result<Tuple<'a>, PgOutputError> {
    let n = p.u16()? as usize;
    let mut cols = Vec::with_capacity(n);
    for _ in 0..n {
        let kind = p.u8()?;
        let col = match kind {
            b'n' => ColumnData::Null,
            b'u' => ColumnData::Unchanged,
            b't' => {
                let len = p.u32()? as usize;
                let bytes = p.take(len)?;
                ColumnData::Text(bytes)
            }
            b'b' => {
                let len = p.u32()? as usize;
                let bytes = p.take(len)?;
                ColumnData::Binary(bytes)
            }
            other => return Err(PgOutputError::UnknownColumnKind(other)),
        };
        cols.push(col);
    }
    Ok(Tuple { columns: cols })
}

fn decode_insert<'a>(p: &mut Parser<'a>) -> Result<Message<'a>, PgOutputError> {
    let relation_oid = p.u32()?;
    // 'N' marker (new tuple) precedes the tuple body.
    let marker = p.u8()?;
    if marker != b'N' {
        return Err(PgOutputError::UnknownKind(marker));
    }
    let tuple = decode_tuple(p)?;
    Ok(Message::Insert { relation_oid, tuple })
}

fn decode_update<'a>(p: &mut Parser<'a>) -> Result<Message<'a>, PgOutputError> {
    let relation_oid = p.u32()?;
    let mut key_old: Option<Tuple<'a>> = None;
    let mut full_old: Option<Tuple<'a>> = None;

    // optional 'K' (key only) or 'O' (full old) before 'N'.
    let first = p.u8()?;
    let new_marker = match first {
        b'K' => {
            key_old = Some(decode_tuple(p)?);
            p.u8()?
        }
        b'O' => {
            full_old = Some(decode_tuple(p)?);
            p.u8()?
        }
        b'N' => first,
        other => return Err(PgOutputError::UnknownKind(other)),
    };
    if new_marker != b'N' {
        return Err(PgOutputError::UnknownKind(new_marker));
    }
    let new = decode_tuple(p)?;
    Ok(Message::Update {
        relation_oid,
        payload: UpdatePayload { key_old, full_old, new },
    })
}

fn decode_delete<'a>(p: &mut Parser<'a>) -> Result<Message<'a>, PgOutputError> {
    let relation_oid = p.u32()?;
    let marker = p.u8()?;
    let tuple = decode_tuple(p)?;
    let payload = match marker {
        b'K' => DeletePayload::KeyOnly(tuple),
        b'O' => DeletePayload::Full(tuple),
        other => return Err(PgOutputError::UnknownKind(other)),
    };
    Ok(Message::Delete { relation_oid, payload })
}

fn decode_truncate(p: &mut Parser<'_>) -> Result<Message<'static>, PgOutputError> {
    let n = p.u32()? as usize;
    let flags = p.u8()?;
    let mut oids = Vec::with_capacity(n);
    for _ in 0..n {
        oids.push(p.u32()?);
    }
    Ok(Message::Truncate(TruncatePayload {
        relation_oids: oids,
        flags,
    }))
}

struct Parser<'a> {
    cur: Cursor<&'a [u8]>,
}

impl<'a> Parser<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { cur: Cursor::new(buf) }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], PgOutputError> {
        let pos = self.cur.position() as usize;
        let buf = *self.cur.get_ref();
        if pos + n > buf.len() {
            return Err(PgOutputError::Truncated);
        }
        let out = &buf[pos..pos + n];
        self.cur.set_position((pos + n) as u64);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, PgOutputError> {
        let mut b = [0u8; 1];
        self.cur.read_exact(&mut b).map_err(|_| PgOutputError::Truncated)?;
        Ok(b[0])
    }

    fn u16(&mut self) -> Result<u16, PgOutputError> {
        let mut b = [0u8; 2];
        self.cur.read_exact(&mut b).map_err(|_| PgOutputError::Truncated)?;
        Ok(u16::from_be_bytes(b))
    }

    fn u32(&mut self) -> Result<u32, PgOutputError> {
        let mut b = [0u8; 4];
        self.cur.read_exact(&mut b).map_err(|_| PgOutputError::Truncated)?;
        Ok(u32::from_be_bytes(b))
    }

    fn i32(&mut self) -> Result<i32, PgOutputError> {
        let mut b = [0u8; 4];
        self.cur.read_exact(&mut b).map_err(|_| PgOutputError::Truncated)?;
        Ok(i32::from_be_bytes(b))
    }

    fn u64(&mut self) -> Result<u64, PgOutputError> {
        let mut b = [0u8; 8];
        self.cur.read_exact(&mut b).map_err(|_| PgOutputError::Truncated)?;
        Ok(u64::from_be_bytes(b))
    }

    fn i64(&mut self) -> Result<i64, PgOutputError> {
        let mut b = [0u8; 8];
        self.cur.read_exact(&mut b).map_err(|_| PgOutputError::Truncated)?;
        Ok(i64::from_be_bytes(b))
    }

    fn cstr(&mut self) -> Result<&'a str, PgOutputError> {
        let pos = self.cur.position() as usize;
        let buf = *self.cur.get_ref();
        let rel = buf[pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or(PgOutputError::MissingNul)?;
        let bytes = &buf[pos..pos + rel];
        let s = std::str::from_utf8(bytes).map_err(|_| PgOutputError::InvalidUtf8)?;
        // skip the NUL too
        self.cur.set_position((pos + rel + 1) as u64);
        Ok(s)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
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
}
