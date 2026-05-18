#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

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

/// `gid_key` mirrors what pgoutput sets when the column is part of the
/// table's effective replica identity (PK under DEFAULT, indexed
/// columns under USING INDEX, or every column under FULL).
fn relation_msg_full(oid: u32, name: &str, replica_identity: u8, gid_key: bool) -> super::Relation {
    super::Relation {
        oid,
        namespace: "public".into(),
        name: name.into(),
        replica_identity,
        columns: vec![
            super::super::pgoutput::RelationColumn {
                flags: if gid_key { 1 } else { 0 },
                name: "gid".into(),
                type_oid: 23,
                type_modifier: -1,
            },
            super::super::pgoutput::RelationColumn {
                // FULL marks every column as key; otherwise only the id
                // columns get the flag. tests that don't care can use
                // the default identity case below.
                flags: if replica_identity == b'f' { 1 } else { 0 },
                name: "geom".into(),
                type_oid: 17_834,
                type_modifier: -1,
            },
        ],
    }
}

/// Standard postgres baseline: REPLICA IDENTITY DEFAULT with `gid`
/// covered by the PRIMARY KEY.
fn relation_msg_with_identity(oid: u32, name: &str, replica_identity: u8) -> super::Relation {
    relation_msg_full(oid, name, replica_identity, true)
}

fn relation_msg() -> super::Relation {
    relation_msg_with_identity(100, "roads_t", b'd')
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
fn rebind_to_oid_without_id_in_identity_marks_rejected() {
    let mut cache = RelationCache::default();
    let t = topo();
    let _ = translate(Message::Relation(relation_msg()), &mut cache, &t).unwrap();

    // operator-side swap with a replacement table whose id column is
    // not part of the replica identity (e.g. no PK, or REPLICA IDENTITY
    // USING INDEX on an index that doesn't cover `gid`).
    let res = translate(
        Message::Relation(relation_msg_full(777, "roads_t", b'd', false)),
        &mut cache,
        &t,
    )
    .unwrap();
    match one_event(res) {
        ChangeEvent::Rebind {
            collection,
            reason: RebindReason::PreflightFailed { reason },
        } => {
            assert_eq!(collection.as_str(), "roads");
            assert!(reason.contains("replica identity"), "reason = {reason}");
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
fn update_emits_new_envelope() {
    let mut cache = RelationCache::default();
    let t = topo();
    let _ = translate(Message::Relation(relation_msg()), &mut cache, &t).unwrap();
    let old_geom = point_le(50.0, 50.0);
    let new_geom = point_le(2000.0, 2000.0);
    // pgoutput may still deliver a full_old tuple (e.g. table happens
    // to be REPLICA IDENTITY FULL); the translator must ignore it and
    // rely on the downstream sidecar for old-side dirty pages.
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
            ..
        } => {
            assert_eq!(feature_id, 42);
            assert_eq!(new_envelope.centroid, [2000.0, 2000.0]);
        }
        other => panic!("expected Update event, got {other:?}"),
    }
}

#[test]
fn update_without_full_old_succeeds_under_default_identity() {
    // standard postgres path: DEFAULT identity → no full_old tuple,
    // just key_old (or nothing when the PK is unchanged). translator
    // recovers feature_id from `new`.
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
        ChangeEvent::Update { feature_id, .. } => {
            assert_eq!(feature_id, 42);
        }
        other => panic!("expected Update event, got {other:?}"),
    }
}

#[test]
fn relation_without_id_in_identity_emits_rebind_preflight_failed() {
    let mut cache = RelationCache::default();
    let t = topo();
    // relation reports `gid` with no key flag - either no PK or a
    // USING INDEX that doesn't cover the id column. preflight refuses
    // at Relation-message time rather than letting the first DELETE
    // hit an unrecoverable feature_id.
    let res = translate(
        Message::Relation(relation_msg_full(100, "roads_t", b'd', false)),
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
            assert!(failure_reason.contains("replica identity"), "reason = {failure_reason}");
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
fn delete_extracts_feature_id_from_either_tuple() {
    let mut cache = RelationCache::default();
    let t = topo();
    let _ = translate(Message::Relation(relation_msg()), &mut cache, &t).unwrap();

    // full identity path: O tuple carries every column. feature_id
    // is recovered from it; the old geometry is dropped on the floor.
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
        ChangeEvent::Delete { feature_id, .. } => {
            assert_eq!(feature_id, 42);
        }
        other => panic!("expected Delete event, got {other:?}"),
    }

    // default identity path: K tuple carries key columns only. the
    // geometry slot is unused (typically NULL); feature_id still
    // comes through.
    let res = translate(
        Message::Delete {
            relation_oid: 100,
            payload: DeletePayload::KeyOnly(Tuple {
                columns: vec![ColumnData::Text(b"99"), ColumnData::Null],
            }),
        },
        &mut cache,
        &t,
    )
    .unwrap();
    match one_event(res) {
        ChangeEvent::Delete { feature_id, .. } => {
            assert_eq!(feature_id, 99);
        }
        other => panic!("expected Delete event, got {other:?}"),
    }
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
