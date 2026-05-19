//! Per-CR `DefinitionSource` poller lifecycle.
//!
//! [`PollerManager`] owns one background tokio task per `MarsService` CR-UID
//! whose `DefinitionSpec` requires polling (`gitRef` / `s3Ref`). The task
//! drives the adapter's [`DefinitionSource::watch`] stream and forwards every
//! [`Change`] event into a `tokio::sync::mpsc::Sender<ReconcileTrigger>` the
//! controller fans into `Controller::reconcile_on`.
//!
//! Lifecycle rules:
//! * `register` is idempotent. Called on every reconcile pass.
//! * Same `(cr_uid, spec)` as already running: no-op.
//! * Same `cr_uid`, different `spec` (adapter swap or content change): cancel
//!   the running poller, spawn a fresh one.
//! * Non-polling variants (`inline`, `configMapRef`): no task is spawned; any
//!   previously running poller for this UID is cancelled (handles e.g. a swap
//!   from `gitRef` → `inline`).
//! * `unregister` cancels and drops the entry.
//! * `Drop` cancels every outstanding poller; the runtime drains the tasks.
//!
//! The manager is intentionally narrow: it knows nothing about the controller
//! `Action` type and does not call back into reconcile directly. The mpsc
//! channel is the only coupling.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use mars_definition_source::DefinitionSource;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::crd::definition::DefinitionSpec;
use crate::definition::{self, ResolveError};

/// Reconcile-trigger payload pushed by a poller task on every adapter
/// [`mars_definition_source::Change`]. The controller fans the receiver
/// side into `kube::runtime::Controller::reconcile_on` so each trigger
/// becomes a reconcile pass for the named CR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReconcileTrigger {
    pub(crate) namespace: String,
    pub(crate) name: String,
}

/// Errors raised by [`PollerManager::register`].
#[derive(Debug, thiserror::Error)]
pub(crate) enum ManagerError {
    #[error(transparent)]
    Resolve(#[from] ResolveError),
}

/// Per-UID lifecycle handle. Dropping it does not stop the task; callers must
/// cancel the token first (the manager always does, via `cancel_and_remove`).
struct RunningPoller {
    spec: DefinitionSpec,
    cancel: CancellationToken,
    join: JoinHandle<()>,
}

/// Owns the per-CR poller table. The kube client is supplied per `register`
/// call (the controller already holds it via `Ctx`) so this struct stays
/// I/O-free and trivially testable without a live cluster.
pub(crate) struct PollerManager {
    inner: Arc<Inner>,
}

struct Inner {
    sink: mpsc::Sender<ReconcileTrigger>,
    pollers: Mutex<HashMap<String, RunningPoller>>,
}

impl PollerManager {
    pub(crate) fn new(sink: mpsc::Sender<ReconcileTrigger>) -> Self {
        Self {
            inner: Arc::new(Inner {
                sink,
                pollers: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Ensure a poller for `cr_uid` matches `spec`. Cancels + respawns on
    /// material change; no-op on identical spec; cancels (without respawning)
    /// for non-polling variants. Safe to call from concurrent reconciles.
    pub(crate) async fn register(
        &self,
        cr_uid: &str,
        namespace: &str,
        name: &str,
        spec: &DefinitionSpec,
        kube: &kube::Client,
    ) -> Result<(), ManagerError> {
        // fast path: identical spec already running -> no-op without touching the adapter resolver.
        if self.matches_running(cr_uid, spec) {
            return Ok(());
        }

        // non-polling variant: stop any running poller and bail.
        if !needs_poller(spec) {
            self.cancel_and_remove(cr_uid);
            return Ok(());
        }

        // resolve before cancelling the old poller so a failed swap leaves the
        // previous task running rather than producing a silent gap.
        let source = definition::resolve(spec, namespace, kube).await?;
        self.spawn_and_replace(cr_uid, namespace, name, spec.clone(), source);
        Ok(())
    }

    /// Cancel + drop the entry for `cr_uid`. No-op if not registered.
    pub(crate) fn unregister(&self, cr_uid: &str) {
        self.cancel_and_remove(cr_uid);
    }

    fn matches_running(&self, cr_uid: &str, spec: &DefinitionSpec) -> bool {
        let guard = match self.inner.pollers.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(), // poisoned mutex; treat as no match so we respawn
        };
        guard.get(cr_uid).is_some_and(|p| &p.spec == spec)
    }

    fn cancel_and_remove(&self, cr_uid: &str) {
        let removed = match self.inner.pollers.lock() {
            Ok(mut g) => g.remove(cr_uid),
            Err(p) => p.into_inner().remove(cr_uid),
        };
        if let Some(p) = removed {
            p.cancel.cancel();
            // detach: the cancel token guarantees the task exits; awaiting here
            // would block reconcile on adapter stream shutdown.
            drop(p.join);
        }
    }

    fn spawn_and_replace(
        &self,
        cr_uid: &str,
        namespace: &str,
        name: &str,
        spec: DefinitionSpec,
        source: Box<dyn DefinitionSource>,
    ) {
        let cancel = CancellationToken::new();
        let task = poll_loop(
            source,
            ReconcileTrigger {
                namespace: namespace.to_string(),
                name: name.to_string(),
            },
            self.inner.sink.clone(),
            cancel.clone(),
        );
        let join = tokio::spawn(task);
        let new_entry = RunningPoller { spec, cancel, join };

        let previous = match self.inner.pollers.lock() {
            Ok(mut g) => g.insert(cr_uid.to_string(), new_entry),
            Err(p) => p.into_inner().insert(cr_uid.to_string(), new_entry),
        };
        if let Some(prev) = previous {
            prev.cancel.cancel();
            drop(prev.join);
        }
    }
}

#[cfg(test)]
impl PollerManager {
    /// Test-only escape hatch: skip [`definition::resolve`] (which requires a
    /// kube `Secret`/`ConfigMap` API) and inject a pre-built adapter. Mirrors
    /// the production swap semantics: identical-spec no-op, otherwise cancel +
    /// respawn.
    pub(crate) fn register_with_source(
        &self,
        cr_uid: &str,
        namespace: &str,
        name: &str,
        spec: DefinitionSpec,
        source: Box<dyn DefinitionSource>,
    ) {
        if self.matches_running(cr_uid, &spec) {
            return;
        }
        if !needs_poller(&spec) {
            self.cancel_and_remove(cr_uid);
            return;
        }
        self.spawn_and_replace(cr_uid, namespace, name, spec, source);
    }

    pub(crate) fn is_registered(&self, cr_uid: &str) -> bool {
        match self.inner.pollers.lock() {
            Ok(g) => g.contains_key(cr_uid),
            Err(p) => p.into_inner().contains_key(cr_uid),
        }
    }

    pub(crate) fn len(&self) -> usize {
        match self.inner.pollers.lock() {
            Ok(g) => g.len(),
            Err(p) => p.into_inner().len(),
        }
    }
}

impl Drop for PollerManager {
    fn drop(&mut self) {
        // cancel every outstanding poller so runtime shutdown drains them.
        // no block_on: cancellation is sync and the tasks exit on their own.
        let drained: Vec<RunningPoller> = match self.inner.pollers.lock() {
            Ok(mut g) => g.drain().map(|(_, p)| p).collect(),
            Err(p) => p.into_inner().drain().map(|(_, p)| p).collect(),
        };
        for p in drained {
            p.cancel.cancel();
        }
    }
}

fn needs_poller(spec: &DefinitionSpec) -> bool {
    spec.git_ref.is_some() || spec.s3_ref.is_some()
}

/// Drain the adapter's change stream and forward every event as a reconcile
/// trigger. Exits cleanly on cancel or when the stream ends (adapter shutdown).
async fn poll_loop(
    source: Box<dyn DefinitionSource>,
    trigger: ReconcileTrigger,
    sink: mpsc::Sender<ReconcileTrigger>,
    cancel: CancellationToken,
) {
    let mut stream = source.watch();
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                debug!(ns = %trigger.namespace, name = %trigger.name, "poller cancelled");
                break;
            }
            next = stream.next() => match next {
                Some(_change) => {
                    if let Err(e) = sink.send(trigger.clone()).await {
                        // receiver gone: controller is shutting down, exit quietly.
                        warn!(error = %e, "reconcile trigger sink closed; poller exiting");
                        break;
                    }
                }
                None => {
                    debug!(ns = %trigger.namespace, name = %trigger.name, "watch stream ended; poller exiting");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
