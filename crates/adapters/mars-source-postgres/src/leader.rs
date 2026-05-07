//! `LeaderLock` impl backed by postgres session-scoped advisory locks.
//!
//! `pg_try_advisory_lock(int8)` is non-blocking and held for the lifetime of
//! the session. We `take` a connection out of the deadpool so it is detached
//! from the pool entirely; on guard drop, the wrapped `ClientWrapper` is
//! dropped, which aborts the connection task, closing the session and
//! releasing the lock at the server. Belt-and-braces: we also try
//! `pg_advisory_unlock` from a detached task before drop, when a tokio
//! runtime is available.

use async_trait::async_trait;
use deadpool_postgres::{ClientWrapper, Object};
use mars_source::{LeaderLock, LeaderLockGuard, SourceError};

use crate::PgSource;

pub(crate) struct PgLeaderLockGuard {
    // option lets Drop hand the client to a detached unlock task while still
    // satisfying drop-glue ordering on early-return paths.
    client: Option<ClientWrapper>,
    key: i64,
}

impl std::fmt::Debug for PgLeaderLockGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgLeaderLockGuard").field("key", &self.key).finish()
    }
}

impl LeaderLockGuard for PgLeaderLockGuard {}

impl Drop for PgLeaderLockGuard {
    fn drop(&mut self) {
        let Some(client) = self.client.take() else {
            return;
        };
        let key = self.key;
        // proactively unlock when a runtime is available; otherwise drop the
        // wrapper and let the aborted conn task close the session.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let _ = client.execute("SELECT pg_advisory_unlock($1)", &[&key]).await;
                    drop(client);
                });
            }
            Err(_) => drop(client),
        }
    }
}

#[async_trait]
impl LeaderLock for PgSource {
    async fn try_acquire(&self, key: i64) -> Result<Option<Box<dyn LeaderLockGuard>>, SourceError> {
        let obj = self.pool().get().await.map_err(|e| SourceError::backend("pool", e))?;
        // detach from the pool: a session-scoped lock survives only on this
        // exact connection, so it must not be returned to the pool while held.
        let client: ClientWrapper = Object::take(obj);

        let row = client
            .query_one("SELECT pg_try_advisory_lock($1)", &[&key])
            .await
            .map_err(|e| SourceError::backend("pg_try_advisory_lock", e))?;
        let acquired: bool = row
            .try_get(0)
            .map_err(|e| SourceError::backend("pg_try_advisory_lock decode", e))?;

        if acquired {
            Ok(Some(Box::new(PgLeaderLockGuard {
                client: Some(client),
                key,
            })))
        } else {
            // not the leader; drop client to close the session
            Ok(None)
        }
    }
}
