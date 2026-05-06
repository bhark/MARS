//! Replication-protocol I/O loop.
//!
//! **Status**: scoped-out for the current crate version. The pinned
//! `tokio-postgres 0.7.17` does not expose:
//!
//! - the `replication=database` startup-message keyword on its `Config`
//!   parser (`Config::FromStr` rejects unknown keys),
//! - the `CopyBothResponse` (`W`) and `CopyData` (`d`) backend messages —
//!   `postgres-protocol 0.6.x` parses `CopyData` for COPY but has no
//!   `CopyBothResponse` variant.
//! - any way to send a raw `START_REPLICATION SLOT ... LOGICAL ...` command
//!   that does not get short-circuited by the higher-level simple-query
//!   parser.
//!
//! Implementing the protocol on top of `connect_raw` requires writing a
//! parallel connection state-machine that decodes raw backend frames and
//! sends `CopyData` payloads ('r' standby status updates) periodically;
//! that is a substantial chunk of work and a likely source of subtle
//! correctness bugs. The pgoutput decoder, WKB extractor, and translator
//! are all complete, fixture-tested, and ready to drop into this loop the
//! moment a transport is in place.
//!
//! For the time being, this module returns a `NotImplemented` error from
//! the public entry point. The compiler's snapshot path is unaffected (the
//! `change_feed` dependency is held but not subscribed; SPEC §8.2.3 covers
//! the bootstrap case).

use std::sync::Arc;

use mars_source::{ChangeSubscription, SourceError};

use super::ReplicationTopology;
use crate::PgConfig;

/// Spawn the replication subscriber task and return the ack-aware subscription.
pub(crate) async fn run(
    _cfg: Arc<PgConfig>,
    _topology: Arc<ReplicationTopology>,
) -> Result<Box<dyn ChangeSubscription>, SourceError> {
    // Hard rule from the operator runbook: this stub is a typed
    // `NotImplemented`, never a panic. Wiring in the rest of the crate
    // (PgSource::subscribe, decoder, translator) is real and tested.
    Err(SourceError::NotImplemented {
        what: "mars-source-postgres::replication::transport: pgoutput protocol I/O \
               (tokio-postgres 0.7.17 lacks replication-mode + CopyBothResponse support)",
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::replication::ReplicationTopology;
    use mars_source::SourceError;

    #[tokio::test]
    async fn transport_reports_not_implemented() {
        let cfg = Arc::new(PgConfig::default());
        let topo = Arc::new(ReplicationTopology {
            collections: vec![],
            bands: vec![],
            max_cells_per_row: 1,
        });
        let r = run(cfg, topo).await;
        assert!(matches!(r, Err(SourceError::NotImplemented { .. })));
    }
}
