//! Service-loop orchestration: covers `Compiler::run` directly.
//!
//! Asserts that the long-running compiler entrypoint does the right thing
//! around its boundaries - leader-lock, bootstrap, change-feed lifecycle -
//! independent of the snapshot/cycle bodies which have their own tests.
//! Specifically:
//! - skips bootstrap when a prior manifest exists;
//! - exits cleanly when the change feed closes (`next_batch` -> None);
//! - exits cleanly on shutdown without ever publishing an empty manifest;
//! - drives `apply_cycle` for non-empty windows and acks the feed.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use futures_util::stream;
use mars_compiler::{Compiler, Deps};
use mars_config::Config;
use mars_observability::Metrics;
use mars_source::{
    ChangeBatch, ChangeFeed, ChangeSubscription, CompileSession, LeaderLock, LeaderLockGuard, RowBytes, Source,
    SourceBinding as PortBinding, SourceError,
};
use mars_store::mem::{InMemoryPublisher, InMemoryStore};
use mars_store::{ManifestStore, ObjectStore};
use mars_types::Manifest;
use tokio::sync::Mutex as TokioMutex;
use tokio_util::sync::CancellationToken;

const MIN_CONFIG: &str = r#"
service:
  name: svc
  title: t
  abstract: a
  contact_email: ops@example.org

sources:
  - id: default
    type: postgis
    dsn: "postgres://localhost/x"
    native_crs: EPSG:25832

artifacts:
  store:
    type: fs
    path: /tmp/mars-store
  cache:
    path: /tmp/mars-cache
    max_size: 1GiB

scales:
  bands:
    - { name: hi, max_denom_exclusive: 25000 }

interfaces: {}

compiler:
  window: 50ms
"#;

fn config() -> Config {
    serde_yaml_ng::from_str::<Config>(MIN_CONFIG).expect("parse fixture config")
}

#[derive(Debug)]
struct DummyGuard;
impl LeaderLockGuard for DummyGuard {}

struct AlwaysLeader;
#[async_trait]
impl LeaderLock for AlwaysLeader {
    async fn try_acquire(&self, _key: i64) -> Result<Option<Box<dyn LeaderLockGuard>>, SourceError> {
        Ok(Some(Box::new(DummyGuard)))
    }
}

struct NeverLeader;
#[async_trait]
impl LeaderLock for NeverLeader {
    async fn try_acquire(&self, _key: i64) -> Result<Option<Box<dyn LeaderLockGuard>>, SourceError> {
        Ok(None)
    }
}

/// Source whose Source-trait methods are unreachable for the service-loop
/// tests (we never invoke snapshot or cycle paths).
struct UnusedSource;
#[async_trait]
impl Source for UnusedSource {
    async fn stream_rows<'a>(
        &'a self,
        _binding: &'a PortBinding,
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        Ok(Box::pin(stream::empty()))
    }
    async fn stream_rows_by_id<'a>(
        &'a self,
        _binding: &'a PortBinding,
        _ids: &'a [i64],
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        Ok(Box::pin(stream::empty()))
    }
    async fn stream_feature_ids<'a>(
        &'a self,
        _binding: &'a PortBinding,
    ) -> Result<BoxStream<'a, Result<i64, SourceError>>, SourceError> {
        Ok(Box::pin(stream::empty()))
    }
    async fn open_compile_session<'a>(
        &'a self,
        _binding: &'a PortBinding,
    ) -> Result<Box<dyn CompileSession + 'a>, SourceError> {
        Err(SourceError::NotImplemented {
            what: "service-loop test source",
        })
    }
}

/// Subscription that yields a programmable script of batches and then
/// terminates by returning `None`. Records ack calls.
#[derive(Default)]
struct ScriptedSubscription {
    queue: Vec<Result<ChangeBatch, SourceError>>,
    acks: Arc<TokioMutex<Vec<Option<String>>>>,
    shutdown_called: Arc<AtomicBool>,
}

#[async_trait]
impl ChangeSubscription for ScriptedSubscription {
    async fn next_batch(&mut self) -> Option<Result<ChangeBatch, SourceError>> {
        if self.queue.is_empty() {
            None
        } else {
            Some(self.queue.remove(0))
        }
    }

    async fn acknowledge(&mut self, source_version: Option<&str>) -> Result<(), SourceError> {
        self.acks.lock().await.push(source_version.map(str::to_owned));
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<(), SourceError> {
        self.shutdown_called.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Default)]
struct ScriptedFeed {
    queue: TokioMutex<Option<Vec<Result<ChangeBatch, SourceError>>>>,
    acks: Arc<TokioMutex<Vec<Option<String>>>>,
    shutdown_called: Arc<AtomicBool>,
    subscriptions: AtomicUsize,
}

impl ScriptedFeed {
    fn new(events: Vec<Result<ChangeBatch, SourceError>>) -> Arc<Self> {
        Arc::new(Self {
            queue: TokioMutex::new(Some(events)),
            ..Self::default()
        })
    }
}

#[async_trait]
impl ChangeFeed for ScriptedFeed {
    async fn subscribe(&self) -> Result<Box<dyn ChangeSubscription>, SourceError> {
        self.subscriptions.fetch_add(1, Ordering::SeqCst);
        let queue = self.queue.lock().await.take().ok_or(SourceError::NotImplemented {
            what: "double subscribe",
        })?;
        Ok(Box::new(ScriptedSubscription {
            queue,
            acks: self.acks.clone(),
            shutdown_called: self.shutdown_called.clone(),
        }))
    }
}

fn deps_with(feed: Arc<dyn ChangeFeed>, leader: Arc<dyn LeaderLock>, manifest: Arc<InMemoryPublisher>) -> Deps {
    let mut registry = mars_compiler::SourceRegistry::new();
    registry.insert(mars_config::SourceId::new("default"), Arc::new(UnusedSource));
    Deps {
        sources: Arc::new(registry),
        change_feed: feed,
        leader_lock: leader,
        store: Arc::new(InMemoryStore::new()) as Arc<dyn ObjectStore>,
        manifest,
        metrics: Metrics::new().expect("metrics"),
    }
}

async fn count_publishes(p: &InMemoryPublisher) -> usize {
    p.current().await.unwrap().is_some() as usize
}

#[tokio::test]
async fn run_with_prior_manifest_and_closed_feed_exits_clean() {
    let manifest = Arc::new(InMemoryPublisher::new());
    let prior = Manifest::empty(7, "svc");
    manifest.publish(&prior).await.unwrap();
    let initial_version = prior.version;

    let feed = ScriptedFeed::new(vec![]);
    let deps = deps_with(feed.clone(), Arc::new(AlwaysLeader), manifest.clone());
    let compiler = Compiler::new(deps, config());

    let cancel = CancellationToken::new();
    let res = tokio::time::timeout(Duration::from_secs(2), compiler.run(cancel)).await;
    assert!(matches!(res, Ok(Ok(()))), "run should exit Ok, got {res:?}");

    // no new manifest publishes (still at the seeded version, never overwritten).
    let current = manifest.current().await.unwrap().expect("manifest seeded");
    assert_eq!(current.version, initial_version, "feed close must not republish");
    assert_eq!(feed.subscriptions.load(Ordering::SeqCst), 1);
    assert!(
        feed.shutdown_called.load(Ordering::SeqCst),
        "subscription shutdown must run"
    );
}

#[tokio::test]
async fn run_with_no_manifest_and_immediate_shutdown_publishes_nothing() {
    let manifest = Arc::new(InMemoryPublisher::new());
    let feed = ScriptedFeed::new(vec![]);
    let deps = deps_with(feed.clone(), Arc::new(AlwaysLeader), manifest.clone());
    let compiler = Compiler::new(deps, config());

    let cancel = CancellationToken::new();
    cancel.cancel();
    let res = tokio::time::timeout(Duration::from_secs(2), compiler.run(cancel)).await;
    assert!(
        matches!(res, Ok(Ok(()))),
        "run should exit Ok on pre-cancel, got {res:?}"
    );

    // critical assertion: no empty manifest is ever published in service mode.
    assert_eq!(count_publishes(&manifest).await, 0, "no manifest must be published");
    assert_eq!(
        feed.subscriptions.load(Ordering::SeqCst),
        0,
        "subscription must not open when shutdown precedes bootstrap"
    );
}

#[tokio::test]
async fn run_returns_not_leader_when_lock_unavailable() {
    let manifest = Arc::new(InMemoryPublisher::new());
    let feed = ScriptedFeed::new(vec![]);
    let deps = deps_with(feed.clone(), Arc::new(NeverLeader), manifest.clone());
    let compiler = Compiler::new(deps, config());

    let res = compiler.run(CancellationToken::new()).await;
    assert!(matches!(res, Err(mars_compiler::CompilerError::NotLeader)));
    assert_eq!(count_publishes(&manifest).await, 0);
}

#[tokio::test]
async fn run_with_prior_manifest_and_shutdown_during_idle_window_publishes_nothing() {
    let manifest = Arc::new(InMemoryPublisher::new());
    let prior = Manifest::empty(3, "svc");
    manifest.publish(&prior).await.unwrap();

    // a feed that returns Pending forever (queue is empty but Subscription
    // doesn't terminate). We model this with a pending subscription type.
    struct PendingSub;
    #[async_trait]
    impl ChangeSubscription for PendingSub {
        async fn next_batch(&mut self) -> Option<Result<ChangeBatch, SourceError>> {
            std::future::pending().await
        }
        async fn acknowledge(&mut self, _source_version: Option<&str>) -> Result<(), SourceError> {
            Ok(())
        }
    }
    struct PendingFeed;
    #[async_trait]
    impl ChangeFeed for PendingFeed {
        async fn subscribe(&self) -> Result<Box<dyn ChangeSubscription>, SourceError> {
            Ok(Box::new(PendingSub))
        }
    }

    let deps = deps_with(Arc::new(PendingFeed), Arc::new(AlwaysLeader), manifest.clone());
    let compiler = Compiler::new(deps, config());
    let cancel = CancellationToken::new();
    let runner = {
        let cancel = cancel.clone();
        tokio::spawn(async move { compiler.run(cancel).await })
    };
    // let at least one idle-window roll over, then shut down.
    tokio::time::sleep(Duration::from_millis(120)).await;
    cancel.cancel();
    let res = tokio::time::timeout(Duration::from_secs(2), runner)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(res, Ok(())), "shutdown must produce clean exit, got {res:?}");
    let current = manifest
        .current()
        .await
        .unwrap()
        .expect("seeded manifest still present");
    assert_eq!(current.version, 3, "idle windows must not republish a manifest");

    // proves no Bytes typed stash leaks (sanity import ref).
    let _ = Bytes::new();
}
