//! Translate decoded pgoutput messages into `ChangeEvent`s.

use std::borrow::Cow;

use mars_source::{ChangeEvent, GeometryEnvelope, SourceError};

use super::pgoutput::{ColumnData, DeletePayload, Message, Relation, Tuple, UpdatePayload};
use super::wkb_bbox::{bbox_of, centroid_of};
use super::{CachedRelation, RelationCache, ReplicationTopology};

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
        Message::Relation(rel) => {
            cache_relation(rel, cache, topology);
            Ok(Translated(Vec::new()))
        }
        Message::Insert { relation_oid, tuple } => insert_event(relation_oid, &tuple, cache, topology),
        Message::Update { relation_oid, payload } => update_event(relation_oid, payload, cache, topology),
        Message::Delete { relation_oid, payload } => delete_event(relation_oid, payload, cache, topology),
        Message::Truncate(t) => truncate_event(&t.relation_oids, cache),
    }
}

fn cache_relation(rel: Relation, cache: &mut RelationCache, topology: &ReplicationTopology) {
    let Some(top) = topology.find(&rel.namespace, &rel.name) else {
        // a relation outside topology means the publication includes more
        // than mars knows about. tolerate but log; row events for it will
        // simply miss the cache and be reported as skipped.
        tracing::warn!(
            namespace = %rel.namespace,
            relation = %rel.name,
            "pgoutput: relation not in mars topology, ignoring"
        );
        return;
    };
    let Some(geom_col_idx) = rel.columns.iter().position(|c| c.name == top.geometry_column) else {
        tracing::error!(
            namespace = %rel.namespace,
            relation = %rel.name,
            geometry_column = %top.geometry_column,
            "pgoutput: relation missing geometry column declared by topology"
        );
        return;
    };
    let Some(id_col_idx) = rel.columns.iter().position(|c| c.name == top.id_column) else {
        tracing::error!(
            namespace = %rel.namespace,
            relation = %rel.name,
            id_column = %top.id_column,
            "pgoutput: relation missing id column declared by topology"
        );
        return;
    };
    let id_type_oid = rel.columns[id_col_idx].type_oid;
    cache.insert(
        rel.oid,
        CachedRelation {
            topology: top.clone(),
            id_col_idx,
            id_type_oid,
            geometry_col_idx: geom_col_idx,
            replica_identity: rel.replica_identity,
        },
    );
}

fn insert_event(
    oid: u32,
    tuple: &Tuple<'_>,
    cache: &RelationCache,
    _topology: &ReplicationTopology,
) -> Result<Translated, SourceError> {
    let Some(entry) = cache.get(oid) else {
        // pgoutput guarantees Relation precedes the first row event for the
        // same oid; an unknown oid is a stream-state error, not a stale cache.
        return Err(SourceError::backend_msg(
            "pgoutput",
            format!("insert for unknown relation oid {oid}"),
        ));
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
    let Some(entry) = cache.get(oid) else {
        return Err(SourceError::backend_msg(
            "pgoutput",
            format!("update for unknown relation oid {oid}"),
        ));
    };

    // SPEC §8.2.1: bound tables MUST carry REPLICA IDENTITY FULL so the
    // OLD geometry is present on every UPDATE/DELETE. phase-c uses both
    // old and new bboxes to derive Hilbert keys covering the dirty pages.
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

fn missing_full_old_error(entry: &CachedRelation, op: &str) -> SourceError {
    let schema = &entry.topology.schema;
    let table = &entry.topology.table;
    if entry.replica_identity == b'f' {
        // pgoutput claims FULL but no O tuple arrived: defensive — should
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
    let Some(entry) = cache.get(oid) else {
        return Err(SourceError::backend_msg(
            "pgoutput",
            format!("delete for unknown relation oid {oid}"),
        ));
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
    // multi-relation truncate: emit one event per known oid. unknown oids
    // belong to relations outside the configured topology and are skipped.
    let mut events = Vec::new();
    for oid in oids {
        if let Some(entry) = cache.get(*oid) {
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
    // TODO: consolidate this duplicate wkb walker with mars-artifact.
    let bbox = bbox_of(&geom).map_err(|e| SourceError::backend("wkb bbox", e))?;
    let centroid = centroid_of(&geom).map_err(|e| SourceError::backend("wkb centroid", e))?;
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
        let entry = cache.get(100).unwrap();
        assert_eq!(entry.geometry_col_idx, 1);
        assert_eq!(entry.topology.collection.as_str(), "roads");
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
    fn update_without_full_old_errors_on_default_identity() {
        let mut cache = RelationCache::default();
        let t = topo();
        // relation has REPLICA IDENTITY DEFAULT (b'd'), not FULL.
        let _ = translate(
            Message::Relation(relation_msg_with_identity(100, "roads_t", b'd')),
            &mut cache,
            &t,
        )
        .unwrap();
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
        match err {
            Err(SourceError::Backend { source, .. }) => {
                let msg = source.to_string();
                assert!(msg.contains("REPLICA IDENTITY FULL"), "msg = {msg}");
                assert!(msg.contains("public.roads_t"), "msg = {msg}");
            }
            other => panic!("expected Backend error, got {other:?}"),
        }
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
