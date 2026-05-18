//! Coordination port: leader lock so at most one compiler instance per
//! service is active at a time.

use async_trait::async_trait;

use crate::SourceError;

/// Opaque guard returned by [`LeaderLock::try_acquire`]. Holding it keeps the
/// underlying lock; dropping it releases the lock (typically by closing the
/// session that owns it).
pub trait LeaderLockGuard: Send + Sync + std::fmt::Debug {}

/// Coordination port: at most one compiler instance per service holds the
/// leader lock at a time. The hash is precomputed by the application so the
/// adapter does not need to know about service identity.
#[async_trait]
pub trait LeaderLock: Send + Sync + 'static {
    /// Try to acquire the lock keyed by `key`. Non-blocking:
    /// - `Ok(Some(guard))` - leader; hold `guard` for the duration of work.
    /// - `Ok(None)` - another instance holds the lock.
    /// - `Err(_)` - backend error.
    async fn try_acquire(&self, key: i64) -> Result<Option<Box<dyn LeaderLockGuard>>, SourceError>;
}
