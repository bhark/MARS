//! service-mode shell: helpers used only by the long-running
//! `Compiler::run` loop. not a pipeline stage and not consumed by
//! one-shot operator entry points; lives here so the orchestration in
//! `lib.rs` stays focused on composition.

use std::time::Duration;

use mars_source::{ChangeBatch, ChangeSubscription};
use tokio_util::sync::CancellationToken;

use crate::CompilerError;

/// Deterministic 64-bit hash of the leader-lock key, reinterpreted as `i64`
/// for `pg_try_advisory_lock`. FNV-1a is stable across releases and has no
/// runtime dependency.
#[must_use]
pub(crate) fn leader_lock_key(name: &str) -> i64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    for &b in name.as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h as i64
}

pub(crate) enum CollectOutcome {
    Batches(Vec<ChangeBatch>),
    FeedClosed,
    Shutdown,
}

/// Drain the subscription until `window` elapses or shutdown fires. Returns
/// every batch that arrived in the window. An empty batch list is a normal
/// idle window.
pub(crate) async fn collect_batches(
    sub: &mut dyn ChangeSubscription,
    window: Duration,
    shutdown: &CancellationToken,
) -> Result<CollectOutcome, CompilerError> {
    let deadline = tokio::time::Instant::now() + window;
    let mut batches: Vec<ChangeBatch> = Vec::new();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Ok(CollectOutcome::Shutdown),
            _ = tokio::time::sleep_until(deadline) => return Ok(CollectOutcome::Batches(batches)),
            next = sub.next_batch() => match next {
                None => return Ok(CollectOutcome::FeedClosed),
                Some(Err(e)) => return Err(CompilerError::Source(e)),
                Some(Ok(b)) => batches.push(b),
            }
        }
    }
}
