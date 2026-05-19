//! port-level fake for `mars_definition_source::DefinitionSource`. lets tests
//! drive both `fetch` (with a current payload + optional one-shot error
//! injection) and `watch` (with hand-rolled `Change` events) without spinning
//! a real adapter. shared by the operator's poller tests and any future
//! reconcile-layer integration.
//!
//! `watch` is single-consumer: the first call takes the underlying mpsc
//! receiver and subsequent calls observe an empty stream. this is sufficient
//! for the production lifecycle (one `poll_loop` per spec) and avoids the
//! broadcast subscribe-before-emit race that bites tests which emit before
//! the poller has had a chance to call `watch()`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use futures_util::{StreamExt, stream};
use mars_definition_source::{Change, DefinitionBytes, DefinitionSource, DefinitionSourceError};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

// generous so chatty tests (rapid set_payload bursts) never see backpressure.
const CHANNEL_CAP: usize = 64;

/// Fake [`DefinitionSource`]: capture-and-replay payload, queueable fetch
/// failures, hand-driven change events.
pub struct FakeDefinitionSource {
    state: Arc<Mutex<State>>,
    tx: mpsc::Sender<Change>,
    rx: Mutex<Option<mpsc::Receiver<Change>>>,
}

struct State {
    data: Bytes,
    revision: String,
    fetch_errors: VecDeque<DefinitionSourceError>,
}

impl FakeDefinitionSource {
    /// Build a fake serving `initial` bytes at `revision` on `fetch`.
    pub fn new(initial: impl Into<Bytes>, revision: impl Into<String>) -> Self {
        let (tx, rx) = mpsc::channel(CHANNEL_CAP);
        Self {
            state: Arc::new(Mutex::new(State {
                data: initial.into(),
                revision: revision.into(),
                fetch_errors: VecDeque::new(),
            })),
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }

    /// Update the payload and queue a [`Change`] carrying the new revision
    /// for the (single) watch consumer.
    pub async fn set_payload(&self, bytes: impl Into<Bytes>, revision: impl Into<String>) {
        let revision = revision.into();
        {
            let mut guard = self.state.lock().expect("poisoned");
            guard.data = bytes.into();
            guard.revision = revision.clone();
        }
        // tx.send may fail if the receiver has been dropped (test torn down or
        // watch() never called). benign for fakes.
        let _ = self.tx.send(Change { revision }).await;
    }

    /// Queue a [`Change`] without altering the payload. Useful for replaying
    /// a stale-but-non-conflicting watch event.
    pub async fn emit_change(&self, revision: impl Into<String>) {
        let _ = self
            .tx
            .send(Change {
                revision: revision.into(),
            })
            .await;
    }

    /// Queue a one-shot error to be returned by the next `fetch`. FIFO order
    /// when multiple are queued.
    pub fn fail_next_fetch(&self, error: DefinitionSourceError) {
        self.state.lock().expect("poisoned").fetch_errors.push_back(error);
    }

    /// Current revision string, for assertions.
    pub fn revision(&self) -> String {
        self.state.lock().expect("poisoned").revision.clone()
    }
}

#[async_trait]
impl DefinitionSource for FakeDefinitionSource {
    async fn fetch(&self) -> Result<DefinitionBytes, DefinitionSourceError> {
        let mut guard = self.state.lock().expect("poisoned");
        if let Some(e) = guard.fetch_errors.pop_front() {
            return Err(e);
        }
        Ok(DefinitionBytes {
            data: guard.data.clone(),
            revision: guard.revision.clone(),
        })
    }

    fn watch(&self) -> BoxStream<'static, Change> {
        match self.rx.lock().expect("poisoned").take() {
            Some(rx) => ReceiverStream::new(rx).boxed(),
            None => stream::empty().boxed(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
