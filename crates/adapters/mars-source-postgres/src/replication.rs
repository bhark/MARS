//! pgoutput logical-decoding subscriber (SPEC §8.2.1).
//!
//! Layout:
//! - `pgoutput`: byte-level decoder for pgoutput v1/v2 messages. Pure, tested
//!   against captured fixtures.
//! - `wkb_bbox`: bbox-only WKB / EWKB extractor. Avoids materialising full
//!   geometries; we only need the cell-touching extents.
//! - `translate`: relation-cache + topology-aware lowering from pgoutput
//!   messages to `ChangeEvent`s.
//! - `transport`: the actual replication-protocol I/O loop (`START_REPLICATION
//!   SLOT ... LOGICAL`, CopyBoth framing, standby status updates). Currently
//!   gated as `NotImplemented` because the pinned `tokio-postgres 0.7.17`
//!   does not expose the `replication=database` connect mode nor a
//!   CopyBothResponse stream; lifting either requires either vendoring a
//!   substantial slice of the connection state machine or pulling in a
//!   heavy crate that does the same. The decoder + translator are wired
//!   so a future transport drops in cleanly.
//!
//! See `subscribe` for the surface called by `PgSource::subscribe`.

use std::collections::HashMap;
use std::sync::Arc;

use mars_grid::{BandConfig, cells_in_bbox};
use mars_source::{ChangeSubscription, SourceError};
use mars_types::Bbox;

// dead_code allowed: these modules are fully tested via their own unit tests
// and wired to be driven by `transport` once the replication protocol I/O is
// in place. silencing here keeps `-D warnings` clean while the wiring is
// the only consumer of the symbols.
#[allow(dead_code)]
pub(crate) mod pgoutput;
#[allow(dead_code)]
pub(crate) mod translate;
pub(crate) mod transport;
#[allow(dead_code)]
pub(crate) mod wkb_bbox;

/// Per-collection topology the change-feed needs to compute dirty cells.
/// Keyed by the fully-qualified `schema.table` (matches what pgoutput's
/// Relation message reports). The geometry column lives in `geometry_column`;
/// the logical `collection` name surfaces in emitted `ChangeEvent`s.
#[derive(Debug, Clone)]
pub struct CollectionTopology {
    /// Logical collection identifier reported in `ChangeEvent`.
    pub collection: String,
    /// Schema name as known to postgres (e.g. `public`).
    pub schema: String,
    /// Table name as known to postgres.
    pub table: String,
    /// Geometry column name in the relation.
    pub geometry_column: String,
}

impl CollectionTopology {
    /// `schema.table` join used as the relation-cache key.
    #[must_use]
    pub fn qualified(&self) -> String {
        format!("{}.{}", self.schema, self.table)
    }
}

/// Wiring `PgSource::subscribe` needs at runtime. Independent of `PgConfig`
/// because the topology is layer-derived; the bin builds it from the parsed
/// config and hands it to `PgSource::with_topology`.
#[derive(Debug, Clone)]
pub struct ReplicationTopology {
    /// One entry per pgoutput-bound collection.
    pub collections: Vec<CollectionTopology>,
    /// Configured scale bands; geometry bbox is enumerated against every
    /// band so all touched cells across all bands are reported as dirty.
    pub bands: Vec<BandConfig>,
    /// Hard ceiling on cells emitted per row to prevent a bug-induced bbox
    /// from generating an unbounded list.
    pub max_cells_per_row: usize,
}

impl ReplicationTopology {
    /// Look up by `schema.table`.
    #[must_use]
    pub fn find(&self, schema: &str, table: &str) -> Option<&CollectionTopology> {
        self.collections.iter().find(|c| c.schema == schema && c.table == table)
    }
}

/// Glue: spawn the replication task and return the ack-aware subscription.
///
/// transport returns `NotImplemented` today. when it gains a real impl, the
/// shape here (bounded mpsc, cancellable spawn) does not change.
pub(crate) async fn subscribe(
    cfg: Arc<crate::PgConfig>,
    topology: Arc<ReplicationTopology>,
) -> Result<Box<dyn ChangeSubscription>, SourceError> {
    transport::run(cfg, topology).await
}

/// Compute the union of cells touched by `bbox` across every configured band.
/// Used by both Insert/Update/Delete event lowering.
#[allow(dead_code)]
pub(crate) fn cells_for_bbox(
    bbox: Bbox,
    bands: &[BandConfig],
    max_per_band: usize,
) -> Result<Vec<mars_types::Cell>, SourceError> {
    let mut out: Vec<mars_types::Cell> = Vec::new();
    let mut seen: std::collections::HashSet<(String, i64, i64)> = std::collections::HashSet::new();
    for band in bands {
        let cells =
            cells_in_bbox(bbox, band, max_per_band).map_err(|e| SourceError::Backend(format!("cells_in_bbox: {e}")))?;
        for cell in cells {
            let key = (cell.band.as_str().to_string(), cell.x, cell.y);
            if seen.insert(key) {
                out.push(cell);
            }
        }
    }
    Ok(out)
}

/// Cache mapping pgoutput relation oid to the `CollectionTopology` plus
/// the index of the geometry column inside the relation's column list. The
/// relation message arrives once per relation per session; we cache it for
/// every subsequent row event referencing that oid.
#[derive(Debug, Default)]
#[allow(dead_code)]
pub(crate) struct RelationCache {
    entries: HashMap<u32, CachedRelation>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct CachedRelation {
    pub topology: CollectionTopology,
    pub geometry_col_idx: usize,
}

#[allow(dead_code)]
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
    use mars_types::ScaleBand;

    fn band(name: &str, max_denom: u32, cell: f64) -> BandConfig {
        BandConfig {
            name: ScaleBand::new(name),
            max_denom,
            origin: (0.0, 0.0),
            cell_size: cell,
        }
    }

    #[test]
    fn cells_for_bbox_unions_across_bands() {
        let bands = vec![band("hi", 25_000, 1024.0), band("med", 100_000, 4096.0)];
        let bbox = Bbox::new(0.0, 0.0, 100.0, 100.0);
        let cells = cells_for_bbox(bbox, &bands, 1_000).unwrap();
        // small bbox: one cell per band
        assert_eq!(cells.len(), 2);
        let band_names: std::collections::HashSet<_> = cells.iter().map(|c| c.band.as_str().to_string()).collect();
        assert!(band_names.contains("hi"));
        assert!(band_names.contains("med"));
    }

    #[test]
    fn cells_for_bbox_dedups_within_band() {
        // single band, single cell - should appear once even after the dedup pass
        let bands = vec![band("hi", 25_000, 1024.0)];
        let bbox = Bbox::new(50.0, 50.0, 60.0, 60.0);
        let cells = cells_for_bbox(bbox, &bands, 100).unwrap();
        assert_eq!(cells.len(), 1);
    }

    #[test]
    fn topology_lookup() {
        let t = ReplicationTopology {
            collections: vec![CollectionTopology {
                collection: "roads".into(),
                schema: "public".into(),
                table: "roads_t".into(),
                geometry_column: "geom".into(),
            }],
            bands: vec![],
            max_cells_per_row: 1024,
        };
        assert!(t.find("public", "roads_t").is_some());
        assert!(t.find("public", "buildings").is_none());
    }
}
