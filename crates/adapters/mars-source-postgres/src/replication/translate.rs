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
    if rel.replica_identity != b'f' {
        return Err(RelationBindError::IncompatibleReplicaIdentity {
            got: rel.replica_identity,
        });
    }
    let id_type_oid = rel.columns[id_col_idx].type_oid;
    Ok(CachedRelation {
        oid: rel.oid,
        topology: top.clone(),
        id_col_idx,
        id_type_oid,
        geometry_col_idx,
        replica_identity: rel.replica_identity,
        state: BindingState::Active,
    })
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RelationBindError {
    #[error("missing geometry column {column:?} declared by topology")]
    MissingGeometryColumn { column: String },
    #[error("missing id column {column:?} declared by topology")]
    MissingIdColumn { column: String },
    #[error("requires REPLICA IDENTITY FULL (got identity {:?})", *got as char)]
    IncompatibleReplicaIdentity { got: u8 },
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
                replica_identity: rel.replica_identity,
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

    // bound tables MUST carry REPLICA IDENTITY FULL so the
    // OLD geometry is present on every UPDATE/DELETE. preflight enforces
    // this at bind time; this remains as a defensive guard for the case
    // where pgoutput claims FULL but omits the O tuple anyway.
    let Some(old) = payload.full_old.as_ref() else {
        return Err(missing_full_old_error(entry, "update"));
    };
    let feature_id = extract_feature_id(&payload.new, entry)?;
    let old_envelope = envelope_from_tuple(old, entry.geometry_col_idx)?;
    let new_envelope = envelope_from_tuple(&payload.new, entry.geometry_col_idx)?;

    Ok(Translated(vec![ChangeEvent::Update {
        collection: entry.topology.collection.clone(),
        feature_id,
        new_envelope,
        old_envelope: Some(old_envelope),
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

fn missing_full_old_error(entry: &CachedRelation, op: &str) -> SourceError {
    let schema = &entry.topology.schema;
    let table = &entry.topology.table;
    if entry.replica_identity == b'f' {
        // pgoutput claims FULL but no O tuple arrived: defensive - should
        // not happen unless the upstream behaviour changes mid-stream.
        SourceError::backend_msg(
            "pgoutput",
            format!("{op} on {schema}.{table} declares REPLICA IDENTITY FULL but old tuple is missing"),
        )
    } else {
        SourceError::backend_msg(
            "pgoutput",
            format!(
                "{op} on {schema}.{table} requires REPLICA IDENTITY FULL (got identity {:?})",
                entry.replica_identity as char
            ),
        )
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
    let tuple = match &payload {
        DeletePayload::Full(t) => t,
        DeletePayload::KeyOnly(_) => return Err(missing_full_old_error(entry, "delete")),
    };
    let feature_id = extract_feature_id(tuple, entry)?;
    let old_envelope = envelope_from_tuple(tuple, entry.geometry_col_idx)?;
    Ok(Translated(vec![ChangeEvent::Delete {
        collection: entry.topology.collection.clone(),
        feature_id,
        old_envelope: Some(old_envelope),
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::replication::{CollectionTopology, ReplicationTopology};

    fn point_le(x: f64, y: f64) -> Vec<u8> {
        let mut v = vec![1u8];
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&x.to_le_bytes());
        v.extend_from_slice(&y.to_le_bytes());
        v
    }

    fn topo() -> ReplicationTopology {
        ReplicationTopology {
            collections: vec![
                CollectionTopology {
                    collection: "roads".into(),
                    schema: "public".into(),
                    table: "roads_t".into(),
                    geometry_column: "geom".into(),
                    id_column: "gid".into(),
                },
                CollectionTopology {
                    collection: "buildings".into(),
                    schema: "public".into(),
                    table: "buildings_t".into(),
                    geometry_column: "geom".into(),
                    id_column: "gid".into(),
                },
            ],
        }
    }

    fn relation_msg_with_identity(oid: u32, name: &str, replica_identity: u8) -> super::Relation {
        super::Relation {
            oid,
            namespace: "public".into(),
            name: name.into(),
            replica_identity,
            columns: vec![
                super::super::pgoutput::RelationColumn {
                    flags: 0,
                    name: "gid".into(),
                    type_oid: 23,
                    type_modifier: -1,
                },
                super::super::pgoutput::RelationColumn {
                    flags: 0,
                    name: "geom".into(),
                    type_oid: 17_834,
                    type_modifier: -1,
                },
            ],
        }
    }

    fn relation_msg() -> super::Relation {
        relation_msg_with_identity(100, "roads_t", b'f')
    }

    fn one_event(t: Translated) -> ChangeEvent {
        let Translated(mut v) = t;
        assert_eq!(v.len(), 1, "expected exactly one event, got {v:?}");
        v.remove(0)
    }

    #[test]
    fn relation_caches_geometry_index() {
        let mut cache = RelationCache::default();
        let t = topo();
        let _ = translate(Message::Relation(relation_msg()), &mut cache, &t).unwrap();
        let entry = cache.get_by_oid(100).unwrap();
        assert_eq!(entry.geometry_col_idx, 1);
        assert_eq!(entry.topology.collection.as_str(), "roads");
        assert!(matches!(entry.state, BindingState::Active));
    }

    #[test]
    fn fresh_bind_emits_no_event() {
        let mut cache = RelationCache::default();
        let t = topo();
        let Translated(events) = translate(Message::Relation(relation_msg()), &mut cache, &t).unwrap();
        assert!(events.is_empty(), "fresh bind should be silent, got {events:?}");
    }

    #[test]
    fn rebind_to_new_oid_emits_oid_changed() {
        // initial bind at oid 100.
        let mut cache = RelationCache::default();
        let t = topo();
        let _ = translate(Message::Relation(relation_msg()), &mut cache, &t).unwrap();

        // operator-side swap: same name, new oid, still FULL identity.
        let res = translate(
            Message::Relation(relation_msg_with_identity(777, "roads_t", b'f')),
            &mut cache,
            &t,
        )
        .unwrap();
        match one_event(res) {
            ChangeEvent::Rebind {
                collection,
                reason: RebindReason::OidChanged { old_oid, new_oid },
            } => {
                assert_eq!(collection.as_str(), "roads");
                assert_eq!(old_oid, 100);
                assert_eq!(new_oid, 777);
            }
            other => panic!("expected Rebind OidChanged, got {other:?}"),
        }
        // the new oid routes to the rebound entry; the old oid is purged.
        assert!(cache.get_by_oid(100).is_none());
        let entry = cache.get_by_oid(777).unwrap();
        assert!(matches!(entry.state, BindingState::Active));
    }

    #[test]
    fn rebind_to_oid_without_full_identity_marks_rejected() {
        let mut cache = RelationCache::default();
        let t = topo();
        let _ = translate(Message::Relation(relation_msg()), &mut cache, &t).unwrap();

        // operator-side swap with a replacement table that forgot to set
        // REPLICA IDENTITY FULL.
        let res = translate(
            Message::Relation(relation_msg_with_identity(777, "roads_t", b'd')),
            &mut cache,
            &t,
        )
        .unwrap();
        match one_event(res) {
            ChangeEvent::Rebind {
                collection,
                reason: RebindReason::PreflightFailed { .. },
            } => {
                assert_eq!(collection.as_str(), "roads");
            }
            other => panic!("expected Rebind PreflightFailed, got {other:?}"),
        }
        // the new oid is in the cache but in Rejected state; the old oid
        // is gone (rebind purged it, then the rejected entry replaced).
        assert!(cache.get_by_oid(100).is_none());
        let entry = cache.get_by_oid(777).unwrap();
        assert!(matches!(entry.state, BindingState::Rejected { .. }));
    }

    #[test]
    fn unchanged_oid_is_silent() {
        // pgoutput is free to re-emit a Relation for the same oid (e.g.
        // after a schema-bump it does not actually care about). idempotent.
        let mut cache = RelationCache::default();
        let t = topo();
        let _ = translate(Message::Relation(relation_msg()), &mut cache, &t).unwrap();
        let Translated(events) = translate(Message::Relation(relation_msg()), &mut cache, &t).unwrap();
        assert!(
            events.is_empty(),
            "re-bind of same oid should be silent, got {events:?}"
        );
    }

    #[test]
    fn insert_decodes_text_mode_hex_geometry() {
        // text-mode pgoutput delivers PostGIS geometry as ASCII hex of the
        // EWKB bytes. translate must round-trip through hex decoding to
        // surface the geometry envelope (phase-c will derive HilbertKey
        // from this same bbox path).
        let mut cache = RelationCache::default();
        let t = topo();
        let _ = translate(Message::Relation(relation_msg()), &mut cache, &t).unwrap();
        let raw = point_le(50.0, 50.0);
        let mut hex = String::new();
        for b in &raw {
            hex.push_str(&format!("{:02x}", b));
        }
        let tuple = Tuple {
            columns: vec![ColumnData::Text(b"42"), ColumnData::Text(hex.as_bytes())],
        };
        let res = translate(
            Message::Insert {
                relation_oid: 100,
                tuple,
            },
            &mut cache,
            &t,
        )
        .unwrap();
        assert!(matches!(one_event(res), ChangeEvent::Insert { .. }));
    }

    #[test]
    fn insert_emits_event_for_known_collection() {
        let mut cache = RelationCache::default();
        let t = topo();
        let _ = translate(Message::Relation(relation_msg()), &mut cache, &t).unwrap();
        let geom = point_le(50.0, 50.0);
        let tuple = Tuple {
            columns: vec![ColumnData::Text(b"42"), ColumnData::Binary(&geom)],
        };
        let res = translate(
            Message::Insert {
                relation_oid: 100,
                tuple,
            },
            &mut cache,
            &t,
        )
        .unwrap();
        match one_event(res) {
            ChangeEvent::Insert {
                collection,
                feature_id,
                new_envelope,
            } => {
                assert_eq!(collection.as_str(), "roads");
                assert_eq!(feature_id, 42);
                assert_eq!(new_envelope.centroid, [50.0, 50.0]);
                assert_eq!(
                    (
                        new_envelope.bbox.min_x,
                        new_envelope.bbox.min_y,
                        new_envelope.bbox.max_x,
                        new_envelope.bbox.max_y,
                    ),
                    (50.0, 50.0, 50.0, 50.0)
                );
            }
            other => panic!("expected Insert event, got {other:?}"),
        }
    }

    #[test]
    fn update_extracts_old_and_new_geometry() {
        let mut cache = RelationCache::default();
        let t = topo();
        let _ = translate(Message::Relation(relation_msg()), &mut cache, &t).unwrap();
        let old_geom = point_le(50.0, 50.0);
        let new_geom = point_le(2000.0, 2000.0);
        let payload = UpdatePayload {
            key_old: None,
            full_old: Some(Tuple {
                columns: vec![ColumnData::Text(b"42"), ColumnData::Binary(&old_geom)],
            }),
            new: Tuple {
                columns: vec![ColumnData::Text(b"42"), ColumnData::Binary(&new_geom)],
            },
        };
        let res = translate(
            Message::Update {
                relation_oid: 100,
                payload,
            },
            &mut cache,
            &t,
        )
        .unwrap();
        match one_event(res) {
            ChangeEvent::Update {
                feature_id,
                new_envelope,
                old_envelope,
                ..
            } => {
                assert_eq!(feature_id, 42);
                assert_eq!(old_envelope.unwrap().centroid, [50.0, 50.0]);
                assert_eq!(new_envelope.centroid, [2000.0, 2000.0]);
            }
            other => panic!("expected Update event, got {other:?}"),
        }
    }

    #[test]
    fn relation_without_full_identity_emits_rebind_preflight_failed() {
        let mut cache = RelationCache::default();
        let t = topo();
        // relation has REPLICA IDENTITY DEFAULT (b'd'), not FULL: the
        // bind is refused at relation-message time instead of waiting
        // for the first UPDATE to kill the subscription.
        let res = translate(
            Message::Relation(relation_msg_with_identity(100, "roads_t", b'd')),
            &mut cache,
            &t,
        )
        .unwrap();
        match one_event(res) {
            ChangeEvent::Rebind {
                collection,
                reason: RebindReason::PreflightFailed { reason: failure_reason },
            } => {
                assert_eq!(collection.as_str(), "roads");
                assert!(
                    failure_reason.contains("REPLICA IDENTITY FULL"),
                    "reason = {failure_reason}"
                );
            }
            other => panic!("expected Rebind PreflightFailed, got {other:?}"),
        }

        // subsequent row events on the rejected oid drop silently rather
        // than killing the subscription.
        let new_geom = point_le(50.0, 50.0);
        let payload = UpdatePayload {
            key_old: None,
            full_old: None,
            new: Tuple {
                columns: vec![ColumnData::Text(b"42"), ColumnData::Binary(&new_geom)],
            },
        };
        let Translated(events) = translate(
            Message::Update {
                relation_oid: 100,
                payload,
            },
            &mut cache,
            &t,
        )
        .unwrap();
        assert!(events.is_empty(), "rejected oid should drop events, got {events:?}");
    }

    #[test]
    fn update_without_full_old_errors_on_full_identity() {
        // defensive: relation declares FULL but pgoutput omitted the O tuple.
        let mut cache = RelationCache::default();
        let t = topo();
        let _ = translate(Message::Relation(relation_msg()), &mut cache, &t).unwrap();
        let new_geom = point_le(50.0, 50.0);
        let payload = UpdatePayload {
            key_old: None,
            full_old: None,
            new: Tuple {
                columns: vec![ColumnData::Text(b"42"), ColumnData::Binary(&new_geom)],
            },
        };
        let err = translate(
            Message::Update {
                relation_oid: 100,
                payload,
            },
            &mut cache,
            &t,
        );
        assert!(matches!(err, Err(SourceError::Backend { .. })));
    }

    #[test]
    fn delete_requires_full_old() {
        let mut cache = RelationCache::default();
        let t = topo();
        let _ = translate(Message::Relation(relation_msg()), &mut cache, &t).unwrap();
        let g = point_le(10.0, 10.0);
        let res = translate(
            Message::Delete {
                relation_oid: 100,
                payload: DeletePayload::Full(Tuple {
                    columns: vec![ColumnData::Text(b"42"), ColumnData::Binary(&g)],
                }),
            },
            &mut cache,
            &t,
        )
        .unwrap();
        match one_event(res) {
            ChangeEvent::Delete {
                feature_id,
                old_envelope,
                ..
            } => {
                assert_eq!(feature_id, 42);
                assert_eq!(old_envelope.unwrap().centroid, [10.0, 10.0]);
            }
            other => panic!("expected Delete event, got {other:?}"),
        }

        // key-only must error
        let err = translate(
            Message::Delete {
                relation_oid: 100,
                payload: DeletePayload::KeyOnly(Tuple {
                    columns: vec![ColumnData::Text(b"42"), ColumnData::Null],
                }),
            },
            &mut cache,
            &t,
        );
        assert!(matches!(err, Err(SourceError::Backend { .. })));
    }

    #[test]
    fn truncate_emits_collection_event() {
        let mut cache = RelationCache::default();
        let t = topo();
        let _ = translate(Message::Relation(relation_msg()), &mut cache, &t).unwrap();
        let res = translate(
            Message::Truncate(super::super::pgoutput::TruncatePayload {
                relation_oids: vec![100],
                flags: 0,
            }),
            &mut cache,
            &t,
        )
        .unwrap();
        assert!(matches!(one_event(res), ChangeEvent::Truncate { .. }));
    }

    #[test]
    fn truncate_emits_one_event_per_known_relation() {
        let mut cache = RelationCache::default();
        let t = topo();
        let _ = translate(Message::Relation(relation_msg()), &mut cache, &t).unwrap();
        let _ = translate(
            Message::Relation(relation_msg_with_identity(200, "buildings_t", b'f')),
            &mut cache,
            &t,
        )
        .unwrap();
        // mix of known + unknown oids.
        let Translated(events) = translate(
            Message::Truncate(super::super::pgoutput::TruncatePayload {
                relation_oids: vec![100, 999, 200],
                flags: 0,
            }),
            &mut cache,
            &t,
        )
        .unwrap();
        let names: Vec<_> = events
            .iter()
            .map(|e| match e {
                ChangeEvent::Truncate { collection } => collection.as_str(),
                _ => panic!("expected only Truncate events"),
            })
            .collect();
        assert_eq!(names, vec!["roads", "buildings"]);
    }

    #[test]
    fn unknown_relation_in_truncate_is_skipped() {
        let mut cache = RelationCache::default();
        let t = topo();
        let Translated(events) = translate(
            Message::Truncate(super::super::pgoutput::TruncatePayload {
                relation_oids: vec![999],
                flags: 0,
            }),
            &mut cache,
            &t,
        )
        .unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn commit_is_a_noop_at_translator() {
        // Commit boundaries are framed by the transport (pgwire-replication
        // surfaces them as a separate event); translate should produce no
        // ChangeEvents for them.
        let mut cache = RelationCache::default();
        let t = topo();
        let Translated(events) = translate(
            Message::Commit {
                flags: 0,
                commit_lsn: 1,
                end_lsn: 999,
                commit_timestamp: 0,
            },
            &mut cache,
            &t,
        )
        .unwrap();
        assert!(events.is_empty());
    }
}
