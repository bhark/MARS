//! Translate decoded pgoutput messages into `ChangeEvent`s.

use std::borrow::Cow;

use mars_artifact::{wkb_bbox, wkb_centroid};
use mars_source::{ChangeEvent, GeometryEnvelope, RebindReason, SourceError};

use super::pgoutput::{ColumnData, DeletePayload, Message, Relation, Tuple, UpdatePayload};
use super::{BindOutcome, BindingState, CachedRelation, CollectionTopology, RelationCache, ReplicationTopology};

/// Zero or more consumer-visible events from a single pgoutput message.
/// row events return one; multi-relation truncate returns one per known
/// relation oid; relation/begin/commit/origin messages return zero
/// (transaction boundaries are framed by the transport).
#[derive(Debug)]
pub(crate) struct Translated(pub Vec<ChangeEvent>);

/// Translate one decoded pgoutput message. on Relation, mutate the cache;
/// on row events, look up the cache and produce a `ChangeEvent`.
pub(crate) fn translate(
    msg: Message<'_>,
    cache: &mut RelationCache,
    topology: &ReplicationTopology,
) -> Result<Translated, SourceError> {
    match msg {
        Message::Begin { .. } | Message::Commit { .. } | Message::Unhandled => Ok(Translated(Vec::new())),
        Message::Relation(rel) => Ok(cache_relation(rel, cache, topology)),
        Message::Insert { relation_oid, tuple } => insert_event(relation_oid, &tuple, cache, topology),
        Message::Update { relation_oid, payload } => update_event(relation_oid, payload, cache, topology),
        Message::Delete { relation_oid, payload } => delete_event(relation_oid, payload, cache, topology),
        Message::Truncate(t) => truncate_event(&t.relation_oids, cache),
    }
}

/// Pure preflight: the structural and contract checks a relation must
/// pass before its oid can be routed by the change-feed. Returns an
/// active `CachedRelation` on success, or a typed error describing why
/// the bind was refused.
///
/// Lifted out of the row-event hot path so the same diagnostic surfaces
/// at Relation-message time (when we can fail closed on one binding)
/// instead of at the first UPDATE / DELETE (when failure killed the
/// whole subscription).
///
/// the binding's id column must be part of the table's replica identity
/// (PRIMARY KEY for DEFAULT identity, or the index named by REPLICA
/// IDENTITY USING INDEX). pgoutput tags those columns with key flag
/// bit 0x01; if the id column lacks that flag we cannot recover the
/// feature id from a DELETE's K tuple, so the bind is refused.
fn validate_relation_for_bind(rel: &Relation, top: &CollectionTopology) -> Result<CachedRelation, RelationBindError> {
    let Some(geometry_col_idx) = rel.columns.iter().position(|c| c.name == top.geometry_column) else {
        return Err(RelationBindError::MissingGeometryColumn {
            column: top.geometry_column.clone(),
        });
    };
    let Some(id_col_idx) = rel.columns.iter().position(|c| c.name == top.id_column) else {
        return Err(RelationBindError::MissingIdColumn {
            column: top.id_column.clone(),
        });
    };
    let id_col = &rel.columns[id_col_idx];
    if id_col.flags & 1 == 0 {
        return Err(RelationBindError::IdColumnNotInIdentity {
            column: top.id_column.clone(),
        });
    }
    let id_type_oid = id_col.type_oid;
    Ok(CachedRelation {
        oid: rel.oid,
        topology: top.clone(),
        id_col_idx,
        id_type_oid,
        geometry_col_idx,
        state: BindingState::Active,
    })
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RelationBindError {
    #[error("missing geometry column {column:?} declared by topology")]
    MissingGeometryColumn { column: String },
    #[error("missing id column {column:?} declared by topology")]
    MissingIdColumn { column: String },
    #[error(
        "id column {column:?} is not part of the table's replica identity; \
         expected it in the PRIMARY KEY or in the index named by REPLICA IDENTITY USING INDEX"
    )]
    IdColumnNotInIdentity { column: String },
}

fn cache_relation(rel: Relation, cache: &mut RelationCache, topology: &ReplicationTopology) -> Translated {
    let Some(top) = topology.find(&rel.namespace, &rel.name) else {
        // a relation outside topology means the publication includes more
        // than mars knows about. tolerate but log; row events for it will
        // simply miss the cache and be reported as skipped.
        tracing::warn!(
            namespace = %rel.namespace,
            relation = %rel.name,
            "pgoutput: relation not in mars topology, ignoring"
        );
        return Translated(Vec::new());
    };
    match validate_relation_for_bind(&rel, top) {
        Ok(entry) => match cache.bind(entry) {
            BindOutcome::Fresh | BindOutcome::UnchangedOid => Translated(Vec::new()),
            BindOutcome::Rebound { old_oid } => {
                tracing::info!(
                    collection = %top.collection,
                    namespace = %rel.namespace,
                    relation = %rel.name,
                    old_oid,
                    new_oid = rel.oid,
                    "pgoutput: rebind detected, signalling per-binding resnapshot"
                );
                Translated(vec![ChangeEvent::Rebind {
                    collection: top.collection.clone(),
                    reason: RebindReason::OidChanged {
                        old_oid,
                        new_oid: rel.oid,
                    },
                }])
            }
        },
        Err(err) => {
            let reason = err.to_string();
            // mark the binding rejected so subsequent row events on this
            // oid drop silently; emit a Rebind { PreflightFailed } so the
            // compiler degrades the binding via the isolation policy.
            cache.bind(CachedRelation {
                oid: rel.oid,
                topology: top.clone(),
                id_col_idx: 0,
                id_type_oid: 0,
                geometry_col_idx: 0,
                state: BindingState::Rejected { reason: reason.clone() },
            });
            tracing::warn!(
                collection = %top.collection,
                namespace = %rel.namespace,
                relation = %rel.name,
                oid = rel.oid,
                %reason,
                "pgoutput: preflight failed on bind/rebind, refusing to route oid"
            );
            Translated(vec![ChangeEvent::Rebind {
                collection: top.collection.clone(),
                reason: RebindReason::PreflightFailed { reason },
            }])
        }
    }
}

fn insert_event(
    oid: u32,
    tuple: &Tuple<'_>,
    cache: &RelationCache,
    _topology: &ReplicationTopology,
) -> Result<Translated, SourceError> {
    let Some(entry) = active_entry(cache, oid, "insert") else {
        return Ok(Translated(Vec::new()));
    };
    let feature_id = extract_feature_id(tuple, entry)?;
    let new_envelope = envelope_from_tuple(tuple, entry.geometry_col_idx)?;
    Ok(Translated(vec![ChangeEvent::Insert {
        collection: entry.topology.collection.clone(),
        feature_id,
        new_envelope,
    }]))
}

fn update_event(
    oid: u32,
    payload: UpdatePayload<'_>,
    cache: &RelationCache,
    _topology: &ReplicationTopology,
) -> Result<Translated, SourceError> {
    let Some(entry) = active_entry(cache, oid, "update") else {
        return Ok(Translated(Vec::new()));
    };
    // pgoutput's new tuple is always present on UPDATE and always
    // carries the PK columns (the preflight check on the key flag
    // guarantees the id column is one of those). the old-side dirty
    // pages are recovered downstream via the page-membership sidecar
    // keyed by feature_id, so we no longer extract OLD geometry here.
    let feature_id = extract_feature_id(&payload.new, entry)?;
    let new_envelope = envelope_from_tuple(&payload.new, entry.geometry_col_idx)?;

    Ok(Translated(vec![ChangeEvent::Update {
        collection: entry.topology.collection.clone(),
        feature_id,
        new_envelope,
    }]))
}

/// Resolve `oid` to an `Active` cache entry, or return `None` after
/// logging once at the appropriate level. `None` on:
/// - unknown oid: pgoutput stream-state error (Relation never arrived);
///   not turned into a SourceError so other bindings keep flowing.
/// - rejected binding: preflight refused the oid; the per-binding Rebind
///   event already informed the compiler. drop silently.
fn active_entry<'a>(cache: &'a RelationCache, oid: u32, op: &'static str) -> Option<&'a CachedRelation> {
    let entry = cache.get_by_oid(oid)?;
    match &entry.state {
        BindingState::Active => Some(entry),
        BindingState::Rejected { reason } => {
            tracing::debug!(
                op,
                oid,
                collection = %entry.topology.collection,
                %reason,
                "pgoutput: dropping row event for rejected binding"
            );
            None
        }
    }
}

fn delete_event(
    oid: u32,
    payload: DeletePayload<'_>,
    cache: &RelationCache,
    _topology: &ReplicationTopology,
) -> Result<Translated, SourceError> {
    let Some(entry) = active_entry(cache, oid, "delete") else {
        return Ok(Translated(Vec::new()));
    };
    // K (default/index identity) and O (full identity) tuples both
    // carry the key columns; the id-column-in-key preflight guarantees
    // feature_id is recoverable from either. old-side dirty pages come
    // from the page-membership sidecar.
    let tuple = match &payload {
        DeletePayload::Full(t) | DeletePayload::KeyOnly(t) => t,
    };
    let feature_id = extract_feature_id(tuple, entry)?;
    Ok(Translated(vec![ChangeEvent::Delete {
        collection: entry.topology.collection.clone(),
        feature_id,
    }]))
}

fn truncate_event(oids: &[u32], cache: &RelationCache) -> Result<Translated, SourceError> {
    // multi-relation truncate: emit one event per known, active oid.
    // unknown oids belong to relations outside the configured topology
    // and are skipped. rejected bindings are skipped too: a TRUNCATE on
    // an already-degraded binding does not need to flip it further.
    let mut events = Vec::new();
    for oid in oids {
        if let Some(entry) = cache.get_by_oid(*oid)
            && matches!(entry.state, BindingState::Active)
        {
            events.push(ChangeEvent::Truncate {
                collection: entry.topology.collection.clone(),
            });
        }
    }
    Ok(Translated(events))
}

/// Extract the geometry bytes from a pgoutput tuple column.
///
/// pgoutput's default proto_version 1 sends column values in text format -
/// for PostGIS geometry that means the type's `out` function output, which
/// is the EWKB hex string (e.g. `0101000020...`). When binary mode is in
/// effect, the bytes are already raw EWKB. We always return a borrowed or
/// owned slice of raw EWKB bytes, normalising the two encodings.
fn extract_geom_bytes<'a>(tuple: &'a Tuple<'a>, idx: usize) -> Result<Cow<'a, [u8]>, SourceError> {
    let col = tuple
        .columns
        .get(idx)
        .ok_or_else(|| SourceError::backend_msg("pgoutput", format!("geom col index {idx} out of range")))?;
    match col {
        ColumnData::Binary(b) => Ok(Cow::Borrowed(b)),
        ColumnData::Text(b) => decode_geom_hex(b).map(Cow::Owned),
        ColumnData::Null => Err(SourceError::backend_msg("pgoutput", "geometry is NULL")),
        ColumnData::Unchanged => Err(SourceError::backend_msg(
            "pgoutput",
            "geometry column is TOAST-unchanged in OLD tuple (REPLICA IDENTITY FULL?)",
        )),
    }
}

fn envelope_from_tuple(tuple: &Tuple<'_>, geom_idx: usize) -> Result<GeometryEnvelope, SourceError> {
    let geom = extract_geom_bytes(tuple, geom_idx)?;
    let bbox = wkb_bbox(&geom).map_err(|e| SourceError::backend("wkb bbox", e))?;
    let centroid = wkb_centroid(&geom).map_err(|e| SourceError::backend("wkb centroid", e))?;
    Ok(GeometryEnvelope { centroid, bbox })
}

fn extract_feature_id(tuple: &Tuple<'_>, entry: &CachedRelation) -> Result<u64, SourceError> {
    let col = tuple.columns.get(entry.id_col_idx).ok_or_else(|| {
        SourceError::backend_msg("pgoutput", format!("id col index {} out of range", entry.id_col_idx))
    })?;
    let signed = match col {
        ColumnData::Text(b) => parse_text_feature_id(b)?,
        ColumnData::Binary(b) => parse_binary_feature_id(b, entry.id_type_oid)?,
        ColumnData::Null => return Err(SourceError::backend_msg("pgoutput", "feature id is NULL")),
        ColumnData::Unchanged => return Err(SourceError::backend_msg("pgoutput", "feature id is TOAST-unchanged")),
    };
    if signed < 0 {
        return Err(SourceError::backend_msg(
            "pgoutput",
            format!("negative feature id rejected: {signed}"),
        ));
    }
    #[allow(clippy::cast_sign_loss)]
    Ok(signed as u64)
}

fn parse_text_feature_id(b: &[u8]) -> Result<i64, SourceError> {
    let s = std::str::from_utf8(b).map_err(|e| SourceError::backend("feature id utf8", e))?;
    s.parse::<i64>()
        .map_err(|e| SourceError::backend("feature id parse", e))
}

fn parse_binary_feature_id(b: &[u8], type_oid: u32) -> Result<i64, SourceError> {
    match type_oid {
        20 => {
            let arr: [u8; 8] = b
                .try_into()
                .map_err(|_| SourceError::backend_msg("feature id binary", "invalid int8 length"))?;
            Ok(i64::from_be_bytes(arr))
        }
        21 => {
            let arr: [u8; 2] = b
                .try_into()
                .map_err(|_| SourceError::backend_msg("feature id binary", "invalid int2 length"))?;
            Ok(i64::from(i16::from_be_bytes(arr)))
        }
        23 => {
            let arr: [u8; 4] = b
                .try_into()
                .map_err(|_| SourceError::backend_msg("feature id binary", "invalid int4 length"))?;
            Ok(i64::from(i32::from_be_bytes(arr)))
        }
        other => Err(SourceError::backend_msg(
            "feature id binary",
            format!("unsupported id type oid: {other}"),
        )),
    }
}

/// Decode PostGIS' text-format EWKB (uppercase or lowercase hex, no prefix)
/// into raw bytes ready for the WKB bbox extractor.
fn decode_geom_hex(s: &[u8]) -> Result<Vec<u8>, SourceError> {
    if !s.len().is_multiple_of(2) {
        return Err(SourceError::backend_msg(
            "pgoutput",
            format!("geometry hex has odd length {}", s.len()),
        ));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in s.chunks_exact(2) {
        out.push((nibble(pair[0])? << 4) | nibble(pair[1])?);
    }
    Ok(out)
}

fn nibble(c: u8) -> Result<u8, SourceError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(SourceError::backend_msg(
            "pgoutput",
            format!("invalid hex digit {:?} in geometry text", c as char),
        )),
    }
}

#[cfg(test)]
mod tests;
