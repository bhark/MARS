//! pgoutput logical-decoding subscriber.
//!
//! Layout:
//! - `pgoutput`: byte-level decoder for pgoutput v1/v2 messages. Pure, tested
//!   against captured fixtures.
//! - `translate`: relation-cache + topology-aware lowering from pgoutput
//!   messages to `ChangeEvent`s.
//! - `transport`: replication-protocol I/O loop. Drives a
//!   `pgwire-replication` client connected with `replication=database`,
//!   handles SCRAM/MD5 auth, TLS, CopyBoth framing, and standby status
//!   updates. XLogData payloads are fed through `pgoutput::decode` and
//!   `translate`; pgoutput Begin/Commit boundaries arrive as separate
//!   events from the library and frame the per-transaction `ChangeBatch`.
//!
//! See `subscribe` for the surface called by `PgSource::subscribe`.

use std::collections::HashMap;
use std::sync::Arc;

use mars_source::{ChangeSubscription, SourceError};

pub(crate) mod pgoutput;
pub(crate) mod translate;
pub(crate) mod transport;

/// Per-collection topology the change-feed needs to compute dirty cells.
/// Keyed by the fully-qualified `schema.table` (matches what pgoutput's
/// Relation message reports). The geometry column lives in `geometry_column`;
/// the logical `collection` name surfaces in emitted `ChangeEvent`s.
#[derive(Debug, Clone)]
pub struct CollectionTopology {
    /// Logical collection identifier reported in `ChangeEvent`.
    pub collection: mars_source::SourceCollectionId,
    /// Schema name as known to postgres (e.g. `public`).
    pub schema: String,
    /// Table name as known to postgres.
    pub table: String,
    /// Geometry column name in the relation.
    pub geometry_column: String,
    /// feature id column name in the relation.
    pub id_column: String,
}

impl CollectionTopology {
    /// `schema.table` join used as the relation-cache key.
    #[must_use]
    pub fn qualified(&self) -> String {
        format!("{}.{}", self.schema, self.table)
    }
}

/// Wiring `PgSource::subscribe` needs at runtime. The topology is layer-derived;
/// the bin builds it from the parsed config and hands it to
/// `PgSource::with_topology`. Page-keyed substrate: the change-feed
/// translator surfaces per-row bbox/centroid in the emitted `ChangeEvent`,
/// so the topology only needs the relation -> collection mapping.
#[derive(Debug, Clone)]
pub struct ReplicationTopology {
    /// One entry per pgoutput-bound collection.
    pub collections: Vec<CollectionTopology>,
}

impl ReplicationTopology {
    /// Look up by `schema.table`.
    #[must_use]
    pub fn find(&self, schema: &str, table: &str) -> Option<&CollectionTopology> {
        self.collections.iter().find(|c| c.schema == schema && c.table == table)
    }
}

/// Glue: spawn the replication task and return the ack-aware subscription.
pub(crate) async fn subscribe(
    cfg: Arc<crate::PgConfig>,
    topology: Arc<ReplicationTopology>,
) -> Result<Box<dyn ChangeSubscription>, SourceError> {
    transport::run(cfg, topology).await
}

// change-feed translator surfaces the row's bbox/centroid in the ChangeEvent
// payload, from which the compiler derives the affected HilbertKey directly.

/// Cache of bound relations. The configured `(schema, table)` pair is
/// the source of truth (the bound-name contract belongs to mars config);
/// the pgoutput relation oid is just the current routing handle. Lookups
/// from row events come in by oid, so we index both ways.
///
/// A name binds to exactly one entry at a time. When a `Relation` message
/// arrives for a known name carrying a different oid, the cache treats
/// it as a rebind: the old oid is purged from the secondary index and
/// the entry is replaced (state = `Active`) or rejected (state =
/// `Rejected`) depending on preflight.
#[derive(Debug, Default)]
pub(crate) struct RelationCache {
    by_name: HashMap<(String, String), CachedRelation>,
    name_for_oid: HashMap<u32, (String, String)>,
}

#[derive(Debug, Clone)]
pub(crate) struct CachedRelation {
    /// current pgoutput handle. mutated on rebind.
    pub oid: u32,
    pub topology: CollectionTopology,
    pub id_col_idx: usize,
    pub id_type_oid: u32,
    pub geometry_col_idx: usize,
    pub state: BindingState,
}

/// Per-binding state. `Rejected` carries the operator-facing reason so
/// the row-event paths can log once on the first event they drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BindingState {
    Active,
    Rejected { reason: String },
}

/// Outcome of `RelationCache::bind` for a fresh `Relation` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BindOutcome {
    /// no prior cache entry for this name; entry installed.
    Fresh,
    /// existing entry already pointed at this oid; nothing changed.
    UnchangedOid,
    /// entry was bound to a different oid; old oid purged, entry
    /// replaced. carries the previous oid for tracing / event payloads.
    Rebound { old_oid: u32 },
}

impl RelationCache {
    /// Install or replace the entry for the given name. On `Rebound`,
    /// the old oid is purged from the secondary index.
    pub(crate) fn bind(&mut self, entry: CachedRelation) -> BindOutcome {
        let key = (entry.topology.schema.clone(), entry.topology.table.clone());
        let new_oid = entry.oid;
        let outcome = if let Some(prior) = self.by_name.get(&key) {
            if prior.oid == new_oid {
                BindOutcome::UnchangedOid
            } else {
                let old_oid = prior.oid;
                self.name_for_oid.remove(&old_oid);
                BindOutcome::Rebound { old_oid }
            }
        } else {
            BindOutcome::Fresh
        };
        self.name_for_oid.insert(new_oid, key.clone());
        self.by_name.insert(key, entry);
        outcome
    }

    /// Look up by the current pgoutput oid. Row events arrive keyed by
    /// oid so this is the hot-path lookup.
    pub(crate) fn get_by_oid(&self, oid: u32) -> Option<&CachedRelation> {
        let key = self.name_for_oid.get(&oid)?;
        self.by_name.get(key)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn topology_lookup() {
        let t = ReplicationTopology {
            collections: vec![CollectionTopology {
                collection: "roads".into(),
                schema: "public".into(),
                table: "roads_t".into(),
                geometry_column: "geom".into(),
                id_column: "gid".into(),
            }],
        };
        assert!(t.find("public", "roads_t").is_some());
        assert!(t.find("public", "buildings").is_none());
    }

    // cells_for_bbox tests retired.
}
