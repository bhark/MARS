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
mod tests;
