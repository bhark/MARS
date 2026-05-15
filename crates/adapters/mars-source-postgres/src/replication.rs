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

/// Cache mapping pgoutput relation oid to the `CollectionTopology` plus
/// the index of the geometry column inside the relation's column list. The
/// relation message arrives once per relation per session; we cache it for
/// every subsequent row event referencing that oid.
#[derive(Debug, Default)]
pub(crate) struct RelationCache {
    entries: HashMap<u32, CachedRelation>,
}

#[derive(Debug, Clone)]
pub(crate) struct CachedRelation {
    pub topology: CollectionTopology,
    pub id_col_idx: usize,
    pub id_type_oid: u32,
    pub geometry_col_idx: usize,
    /// pgoutput replica-identity byte: `d` default, `n` nothing, `f` full,
    /// `i` index. used by the translator to produce a useful operator-facing
    /// error when an UPDATE/DELETE arrives without the old tuple we need.
    pub replica_identity: u8,
}

impl RelationCache {
    pub(crate) fn insert(&mut self, oid: u32, entry: CachedRelation) {
        self.entries.insert(oid, entry);
    }

    pub(crate) fn get(&self, oid: u32) -> Option<&CachedRelation> {
        self.entries.get(&oid)
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
