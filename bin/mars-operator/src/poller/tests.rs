#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use futures_util::StreamExt;
use mars_definition_source::{Change, DefinitionBytes, DefinitionSource, DefinitionSourceError};
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::*;
use crate::crd::definition::{ConfigMapKeyRef, GitRef, GitRevision, S3Ref, SecretRef};

// adapter-side change feed: the test holds the sender and emits Changes; the
// poll loop subscribes via watch(). uses a Mutex<Option<Receiver>> so the
// single mpsc receiver hands off to whichever watch() call wins (sufficient
// for these tests: only one poller per source at a time).
struct FakeSource {
    revision: String,
    rx: std::sync::Mutex<Option<mpsc::Receiver<Change>>>,
    drop_count: Arc<AtomicUsize>,
}

impl FakeSource {
    fn new(revision: &str) -> (Self, mpsc::Sender<Change>) {
        let (tx, rx) = mpsc::channel(16);
        let drop_count = Arc::new(AtomicUsize::new(0));
        (
            Self {
                revision: revision.into(),
                rx: std::sync::Mutex::new(Some(rx)),
                drop_count,
            },
            tx,
        )
    }

    fn drop_count(&self) -> Arc<AtomicUsize> {
        self.drop_count.clone()
    }
}

// stream that decrements an Arc<AtomicUsize> on Drop so tests can verify the
// poll loop actually exits (and thus drops the stream) on cancel.
struct CountedStream {
    inner: BoxStream<'static, Change>,
    drops: Arc<AtomicUsize>,
}

impl futures_core::Stream for CountedStream {
    type Item = Change;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.inner.poll_next_unpin(cx)
    }
}

impl Drop for CountedStream {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl DefinitionSource for FakeSource {
    async fn fetch(&self) -> Result<DefinitionBytes, DefinitionSourceError> {
        Ok(DefinitionBytes {
            data: Bytes::from_static(b""),
            revision: self.revision.clone(),
        })
    }

    fn watch(&self) -> BoxStream<'static, Change> {
        let rx = self
            .rx
            .lock()
            .expect("poisoned")
            .take()
            .unwrap_or_else(|| mpsc::channel(1).1);
        let inner = tokio_stream::wrappers::ReceiverStream::new(rx).boxed();
        CountedStream {
            inner,
            drops: self.drop_count.clone(),
        }
        .boxed()
    }
}

fn spec_inline(payload: &str) -> DefinitionSpec {
    DefinitionSpec {
        inline: Some(payload.into()),
        ..Default::default()
    }
}

fn spec_config_map(name: &str, key: &str) -> DefinitionSpec {
    DefinitionSpec {
        config_map_ref: Some(ConfigMapKeyRef {
            name: name.into(),
            key: key.into(),
        }),
        ..Default::default()
    }
}

fn spec_git(branch: &str) -> DefinitionSpec {
    DefinitionSpec {
        git_ref: Some(GitRef {
            url: "https://example.com/repo.git".into(),
            git_ref: GitRevision {
                branch: Some(branch.into()),
                ..Default::default()
            },
            path: "def.yaml".into(),
            interval: None,
            secret_ref: Some(SecretRef { name: "fake".into() }),
        }),
        ..Default::default()
    }
}

fn spec_s3() -> DefinitionSpec {
    DefinitionSpec {
        s3_ref: Some(S3Ref {
            endpoint: None,
            region: "us-east-1".into(),
            bucket: "b".into(),
            key: "k".into(),
            interval: None,
            secret_ref: None,
        }),
        ..Default::default()
    }
}

fn manager() -> (PollerManager, mpsc::Receiver<ReconcileTrigger>) {
    let (tx, rx) = mpsc::channel(16);
    (PollerManager::new(tx), rx)
}

// ---- register lifecycle -----------------------------------------------------

#[tokio::test]
async fn inline_spec_does_not_spawn() {
    let (mgr, _rx) = manager();
    mgr.register_with_source(
        "uid-1",
        "ns",
        "svc",
        spec_inline("payload"),
        Box::new(FakeSource::new("r1").0),
    );
    assert!(!mgr.is_registered("uid-1"));
    assert_eq!(mgr.len(), 0);
}

#[tokio::test]
async fn configmap_spec_does_not_spawn() {
    let (mgr, _rx) = manager();
    mgr.register_with_source(
        "uid-1",
        "ns",
        "svc",
        spec_config_map("c", "k"),
        Box::new(FakeSource::new("r1").0),
    );
    assert!(!mgr.is_registered("uid-1"));
}

#[tokio::test]
async fn git_spec_spawns_and_tracks() {
    let (mgr, _rx) = manager();
    let (src, _tx) = FakeSource::new("r1");
    mgr.register_with_source("uid-1", "ns", "svc", spec_git("main"), Box::new(src));
    assert!(mgr.is_registered("uid-1"));
    assert_eq!(mgr.len(), 1);
}

#[tokio::test]
async fn identical_spec_is_noop() {
    let (mgr, _rx) = manager();
    let (src1, _tx1) = FakeSource::new("r1");
    let drops1 = src1.drop_count();
    mgr.register_with_source("uid-1", "ns", "svc", spec_git("main"), Box::new(src1));

    let (src2, _tx2) = FakeSource::new("r1");
    mgr.register_with_source("uid-1", "ns", "svc", spec_git("main"), Box::new(src2));

    // give a brief moment to ensure if a respawn happened the old stream would drop.
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(mgr.len(), 1);
    assert_eq!(
        drops1.load(Ordering::SeqCst),
        0,
        "first stream must not have been dropped"
    );
}

#[tokio::test]
async fn different_spec_swaps_poller() {
    let (mgr, _rx) = manager();
    let (src1, _tx1) = FakeSource::new("r1");
    let drops1 = src1.drop_count();
    mgr.register_with_source("uid-1", "ns", "svc", spec_git("main"), Box::new(src1));

    let (src2, _tx2) = FakeSource::new("r2");
    mgr.register_with_source("uid-1", "ns", "svc", spec_git("release"), Box::new(src2));

    // wait for the cancelled first task to wind down and drop its stream.
    timeout(Duration::from_secs(1), async {
        while drops1.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("first stream dropped after swap");
    assert_eq!(mgr.len(), 1);
}

#[tokio::test]
async fn adapter_swap_git_to_inline_cancels() {
    let (mgr, _rx) = manager();
    let (src1, _tx1) = FakeSource::new("r1");
    let drops1 = src1.drop_count();
    mgr.register_with_source("uid-1", "ns", "svc", spec_git("main"), Box::new(src1));

    mgr.register_with_source(
        "uid-1",
        "ns",
        "svc",
        spec_inline("payload"),
        Box::new(FakeSource::new("ignored").0),
    );

    timeout(Duration::from_secs(1), async {
        while drops1.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("git poller cancelled on swap to inline");
    assert!(!mgr.is_registered("uid-1"));
}

#[tokio::test]
async fn unregister_cancels_and_removes() {
    let (mgr, _rx) = manager();
    let (src, _tx) = FakeSource::new("r1");
    let drops = src.drop_count();
    mgr.register_with_source("uid-1", "ns", "svc", spec_s3(), Box::new(src));

    mgr.unregister("uid-1");

    timeout(Duration::from_secs(1), async {
        while drops.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("stream dropped after unregister");
    assert!(!mgr.is_registered("uid-1"));
}

#[tokio::test]
async fn unregister_unknown_uid_is_noop() {
    let (mgr, _rx) = manager();
    mgr.unregister("never-registered");
    assert_eq!(mgr.len(), 0);
}

// ---- change forwarding ------------------------------------------------------

#[tokio::test]
async fn change_event_becomes_reconcile_trigger() {
    let (mgr, mut rx) = manager();
    let (src, tx) = FakeSource::new("r1");
    mgr.register_with_source("uid-1", "ns-a", "svc-a", spec_git("main"), Box::new(src));

    tx.send(Change { revision: "r2".into() })
        .await
        .expect("receiver present");

    let got = timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("trigger delivered")
        .expect("channel open");
    assert_eq!(got.namespace, "ns-a");
    assert_eq!(got.name, "svc-a");
}

#[tokio::test]
async fn poller_exits_on_sink_close() {
    let (mgr, rx) = manager();
    let (src, tx) = FakeSource::new("r1");
    let drops = src.drop_count();
    mgr.register_with_source("uid-1", "ns", "svc", spec_git("main"), Box::new(src));

    // drop receiver so sink.send returns Err on the next event.
    drop(rx);
    tx.send(Change { revision: "r2".into() })
        .await
        .expect("receiver present");

    timeout(Duration::from_secs(1), async {
        while drops.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("poller exited on sink close");
}

// ---- Drop -------------------------------------------------------------------

#[tokio::test]
async fn drop_cancels_all_pollers() {
    let (mgr, _rx) = manager();
    let (a, _tx_a) = FakeSource::new("a");
    let (b, _tx_b) = FakeSource::new("b");
    let drops_a = a.drop_count();
    let drops_b = b.drop_count();
    mgr.register_with_source("uid-a", "ns", "a", spec_git("main"), Box::new(a));
    mgr.register_with_source("uid-b", "ns", "b", spec_s3(), Box::new(b));
    assert_eq!(mgr.len(), 2);

    drop(mgr);

    timeout(Duration::from_secs(1), async {
        while drops_a.load(Ordering::SeqCst) == 0 || drops_b.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("all pollers cancelled on manager Drop");
}

// ---- variant classification -------------------------------------------------

#[test]
fn needs_poller_inline_false() {
    assert!(!needs_poller(&spec_inline("x")));
}

#[test]
fn needs_poller_configmap_false() {
    assert!(!needs_poller(&spec_config_map("c", "k")));
}

#[test]
fn needs_poller_git_true() {
    assert!(needs_poller(&spec_git("main")));
}

#[test]
fn needs_poller_s3_true() {
    assert!(needs_poller(&spec_s3()));
}
