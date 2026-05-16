//! Polled-etag change feed.
//!
//! Stubbed for v1: a real implementation needs the planner to register
//! `(collection, uri)` pairs at construct time so the polling loop can
//! HEAD each tracked URI on a configured cadence and emit
//! `ChangeEvent::Rebind` when an etag transitions.
//!
//! The shape is sketched here so the bin-shared factory can wire it
//! later without a port-level refactor.

use async_trait::async_trait;
use mars_source::{ChangeBatch, ChangeSubscription, SourceError};

/// Placeholder subscription. Returns `NotImplemented` until the planner
/// learns to register the binding -> URI map.
#[allow(dead_code)]
pub(crate) struct PolledEtagSubscription;

#[async_trait]
impl ChangeSubscription for PolledEtagSubscription {
    async fn next_batch(&mut self) -> Option<Result<ChangeBatch, SourceError>> {
        Some(Err(SourceError::NotImplemented {
            what: "mars-source-vectorfile::PolledEtagSubscription::next_batch",
        }))
    }

    async fn acknowledge(&mut self, _source_version: Option<&str>) -> Result<(), SourceError> {
        // polling fallback has no cursor; ack is a no-op.
        Ok(())
    }
}
