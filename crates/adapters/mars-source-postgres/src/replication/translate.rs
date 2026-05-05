//! Translate decoded pgoutput messages into `ChangeEvent`s.

use mars_source::{ChangeEvent, SourceError};

use super::pgoutput::{ColumnData, DeletePayload, Message, Relation, Tuple, UpdatePayload};
use super::wkb_bbox::bbox_of;
use super::{CachedRelation, RelationCache, ReplicationTopology, cells_for_bbox};

/// Decision returned to the loop. relation messages, type/origin frames, and
/// transaction boundaries are not events; they're cache or no-op signals.
#[derive(Debug)]
pub(crate) enum Translated {
    /// A consumer-visible event was produced.
    Event(ChangeEvent),
    /// No event but the loop should keep going.
    Skip,
    /// The Commit's `end_lsn`. Used by the transport to advance flushed LSN.
    Committed { end_lsn: u64 },
}

/// Translate one decoded pgoutput message. on Relation, mutate the cache;
/// on row events, look up the cache and produce a `ChangeEvent`.
pub(crate) fn translate(
    msg: Message<'_>,
    cache: &mut RelationCache,
    topology: &ReplicationTopology,
) -> Result<Translated, SourceError> {
    match msg {
        Message::Begin { .. } | Message::Unhandled => Ok(Translated::Skip),
        Message::Commit { end_lsn, .. } => Ok(Translated::Committed { end_lsn }),
        Message::Relation(rel) => {
            cache_relation(rel, cache, topology);
            Ok(Translated::Skip)
        }
        Message::Insert { relation_oid, tuple } => row_event(relation_oid, &tuple, cache, topology, EventKind::Insert),
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
    cache.insert(
        rel.oid,
        CachedRelation {
            topology: top.clone(),
            geometry_col_idx: geom_col_idx,
        },
    );
}

#[derive(Copy, Clone)]
enum EventKind {
    Insert,
    Delete,
}

fn row_event(
    oid: u32,
    tuple: &Tuple<'_>,
    cache: &RelationCache,
    topology: &ReplicationTopology,
    kind: EventKind,
) -> Result<Translated, SourceError> {
    let Some(entry) = cache.get(oid) else {
        // unknown relation oid: the loop has not yet seen its Relation
        // message. pgoutput guarantees Relation precedes the first row event
        // referencing the same oid, so this is a stream-state error.
        return Err(SourceError::Backend(format!(
            "pgoutput: row for unknown relation oid {oid}"
        )));
    };
    let geom = extract_geom_bytes(tuple, entry.geometry_col_idx)?;
    let bbox = bbox_of(geom).map_err(|e| SourceError::Backend(format!("wkb: {e}")))?;
    let cells = cells_for_bbox(bbox, &topology.bands, topology.max_cells_per_row)?;
    let ev = match kind {
        EventKind::Insert => ChangeEvent::Insert {
            collection: entry.topology.collection.clone(),
            cells,
        },
        EventKind::Delete => ChangeEvent::Delete {
            collection: entry.topology.collection.clone(),
            cells,
        },
    };
    Ok(Translated::Event(ev))
}

fn update_event(
    oid: u32,
    payload: UpdatePayload<'_>,
    cache: &RelationCache,
    topology: &ReplicationTopology,
) -> Result<Translated, SourceError> {
    let Some(entry) = cache.get(oid) else {
        return Err(SourceError::Backend(format!(
            "pgoutput: update for unknown relation oid {oid}"
        )));
    };
    let mut cells: Vec<mars_types::Cell> = Vec::new();
    let mut seen: std::collections::HashSet<(String, i64, i64)> = std::collections::HashSet::new();

    // OLD bbox: full_old has priority (REPLICA IDENTITY FULL); key_old does
    // not carry geometry, so its absence simply means the OLD bbox is
    // unavailable. without it, deletes-of-moved-geometry leak. SPEC §8.2.1
    // mandates REPLICA IDENTITY FULL for bound tables.
    if let Some(old) = payload.full_old.as_ref()
        && let Ok(geom) = extract_geom_bytes(old, entry.geometry_col_idx)
        && let Ok(bbox) = bbox_of(geom)
    {
        let old_cells = cells_for_bbox(bbox, &topology.bands, topology.max_cells_per_row)?;
        for c in old_cells {
            let k = (c.band.as_str().to_string(), c.x, c.y);
            if seen.insert(k) {
                cells.push(c);
            }
        }
    }

    // NEW bbox is always present.
    let new_geom = extract_geom_bytes(&payload.new, entry.geometry_col_idx)?;
    if let Ok(bbox) = bbox_of(new_geom) {
        let new_cells = cells_for_bbox(bbox, &topology.bands, topology.max_cells_per_row)?;
        for c in new_cells {
            let k = (c.band.as_str().to_string(), c.x, c.y);
            if seen.insert(k) {
                cells.push(c);
            }
        }
    }

    if cells.is_empty() {
        // an update where neither bbox is decodable is a hard error: the
        // dependent invalidation cannot be computed correctly.
        return Err(SourceError::Backend(
            "pgoutput: update produced no decodable bbox (REPLICA IDENTITY FULL configured?)".into(),
        ));
    }

    Ok(Translated::Event(ChangeEvent::Update {
        collection: entry.topology.collection.clone(),
        cells,
    }))
}

fn delete_event(
    oid: u32,
    payload: DeletePayload<'_>,
    cache: &RelationCache,
    topology: &ReplicationTopology,
) -> Result<Translated, SourceError> {
    let Some(entry) = cache.get(oid) else {
        return Err(SourceError::Backend(format!(
            "pgoutput: delete for unknown relation oid {oid}"
        )));
    };
    let tuple = match &payload {
        DeletePayload::Full(t) => t,
        DeletePayload::KeyOnly(_) => {
            return Err(SourceError::Backend(
                "pgoutput: delete carried key-only old row; REPLICA IDENTITY FULL required for correctness".into(),
            ));
        }
    };
    let geom = extract_geom_bytes(tuple, entry.geometry_col_idx)?;
    let bbox = bbox_of(geom).map_err(|e| SourceError::Backend(format!("wkb: {e}")))?;
    let cells = cells_for_bbox(bbox, &topology.bands, topology.max_cells_per_row)?;
    Ok(Translated::Event(ChangeEvent::Delete {
        collection: entry.topology.collection.clone(),
        cells,
    }))
}

fn truncate_event(oids: &[u32], cache: &RelationCache) -> Result<Translated, SourceError> {
    // multi-relation truncate: emit one event per known oid. unknown oids are
    // silently skipped — they belong to relations outside our topology.
    // first known oid wins; subsequent ones are still emitted by the loop
    // because translate is called once per pgoutput message and one message
    // can yield multiple events. for v1 we keep it simple and emit just the
    // first known one; multi-emit can be added by lifting Translated to a
    // Vec.
    for oid in oids {
        if let Some(entry) = cache.get(*oid) {
            return Ok(Translated::Event(ChangeEvent::Truncate {
                collection: entry.topology.collection.clone(),
            }));
        }
    }
    Ok(Translated::Skip)
}

fn extract_geom_bytes<'a>(tuple: &'a Tuple<'a>, idx: usize) -> Result<&'a [u8], SourceError> {
    let col = tuple
        .columns
        .get(idx)
        .ok_or_else(|| SourceError::Backend(format!("pgoutput: geom col index {idx} out of range")))?;
    match col {
        ColumnData::Binary(b) | ColumnData::Text(b) => Ok(b),
        ColumnData::Null => Err(SourceError::Backend("pgoutput: geometry is NULL".into())),
        ColumnData::Unchanged => Err(SourceError::Backend(
            "pgoutput: geometry column is TOAST-unchanged in OLD tuple (REPLICA IDENTITY FULL?)".into(),
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::replication::{CollectionTopology, ReplicationTopology};
    use mars_grid::BandConfig;
    use mars_types::ScaleBand;

    fn point_le(x: f64, y: f64) -> Vec<u8> {
        let mut v = vec![1u8];
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&x.to_le_bytes());
        v.extend_from_slice(&y.to_le_bytes());
        v
    }

    fn topo() -> ReplicationTopology {
        ReplicationTopology {
            collections: vec![CollectionTopology {
                collection: "roads".into(),
                schema: "public".into(),
                table: "roads_t".into(),
                geometry_column: "geom".into(),
            }],
            bands: vec![BandConfig {
                name: ScaleBand::new("hi"),
                max_denom: 25_000,
                origin: (0.0, 0.0),
                cell_size: 1024.0,
            }],
            max_cells_per_row: 1024,
        }
    }

    fn relation_msg() -> super::Relation {
        super::Relation {
            oid: 100,
            namespace: "public".into(),
            name: "roads_t".into(),
            replica_identity: b'f',
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

    #[test]
    fn relation_caches_geometry_index() {
        let mut cache = RelationCache::default();
        let t = topo();
        let _ = translate(Message::Relation(relation_msg()), &mut cache, &t).unwrap();
        let entry = cache.get(100).unwrap();
        assert_eq!(entry.geometry_col_idx, 1);
        assert_eq!(entry.topology.collection, "roads");
    }

    #[test]
    fn insert_emits_event_with_cells() {
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
        match res {
            Translated::Event(ChangeEvent::Insert { collection, cells }) => {
                assert_eq!(collection, "roads");
                assert_eq!(cells.len(), 1);
                assert_eq!(cells[0].band.as_str(), "hi");
            }
            other => panic!("expected Insert event, got {other:?}"),
        }
    }

    #[test]
    fn update_unions_old_and_new_cells() {
        let mut cache = RelationCache::default();
        let t = topo();
        let _ = translate(Message::Relation(relation_msg()), &mut cache, &t).unwrap();
        let old_geom = point_le(50.0, 50.0); // (0,0) cell
        let new_geom = point_le(2000.0, 2000.0); // (1,1) cell
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
        match res {
            Translated::Event(ChangeEvent::Update { cells, .. }) => {
                assert_eq!(cells.len(), 2);
            }
            other => panic!("expected Update, got {other:?}"),
        }
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
        assert!(matches!(res, Translated::Event(ChangeEvent::Delete { .. })));

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
        assert!(matches!(err, Err(SourceError::Backend(_))));
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
        assert!(matches!(res, Translated::Event(ChangeEvent::Truncate { .. })));
    }

    #[test]
    fn unknown_relation_in_truncate_is_skipped() {
        let mut cache = RelationCache::default();
        let t = topo();
        let res = translate(
            Message::Truncate(super::super::pgoutput::TruncatePayload {
                relation_oids: vec![999],
                flags: 0,
            }),
            &mut cache,
            &t,
        )
        .unwrap();
        assert!(matches!(res, Translated::Skip));
    }

    #[test]
    fn commit_returns_end_lsn() {
        let mut cache = RelationCache::default();
        let t = topo();
        let res = translate(
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
        assert!(matches!(res, Translated::Committed { end_lsn: 999 }));
    }
}
