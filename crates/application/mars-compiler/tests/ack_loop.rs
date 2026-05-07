#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support {
    pub(crate) mod mem_leader;
    pub(crate) mod mem_source;
}

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use mars_compiler::{Compiler, Deps};
use mars_config::{
    ArtifactCache, ArtifactStore, Artifacts, Cells, Class, ClassStyle, Compiler as CompilerCfg, Config, Interfaces,
    Layer, Scales, ServiceMeta, Source as CfgSource, SourceBinding as CfgBinding, model::Band,
};
use mars_source::{
    AttrValue, ChangeBatch, ChangeEvent, ChangeFeed, ChangeSubscription, RowBytes, SourceCollectionId, SourceError,
};
use mars_store::mem::{InMemoryPublisher, InMemoryStore};
use mars_store::{ManifestStore, ObjectStore, StoreError};
use mars_types::{Bbox, Cell, CrsCode, LayerId, Manifest, ScaleBand};
use tokio_util::sync::CancellationToken;

use crate::support::mem_leader::MemLeader;
use crate::support::mem_source::{MemSource, wkb_polygon};

/// Programmable [`ChangeSubscription`] used to drive `Compiler::run`.
///
/// Yields a fixed sequence of batches, then parks indefinitely so the run loop
/// only exits when the supplied `CancellationToken` fires. Records every
/// `acknowledge` call into a shared vector so tests can assert on the cursor
/// values that were durably committed.
struct ScriptedSub {
    batches: VecDeque<Result<ChangeBatch, SourceError>>,
    acks: Arc<Mutex<Vec<Option<String>>>>,
}

#[async_trait]
impl ChangeSubscription for ScriptedSub {
    async fn next_batch(&mut self) -> Option<Result<ChangeBatch, SourceError>> {
        if let Some(b) = self.batches.pop_front() {
            return Some(b);
        }
        // simulate a quiet feed: park instead of closing, so the compiler's
        // window deadline drives the next iteration.
        std::future::pending().await
    }

    async fn acknowledge(&mut self, source_version: Option<&str>) -> Result<(), SourceError> {
        self.acks
            .lock()
            .expect("acks poisoned")
            .push(source_version.map(str::to_owned));
        Ok(())
    }
}

struct ScriptedFeed {
    sub: Mutex<Option<ScriptedSub>>,
}

#[async_trait]
impl ChangeFeed for ScriptedFeed {
    async fn subscribe(&self) -> Result<Box<dyn ChangeSubscription>, SourceError> {
        let sub = self
            .sub
            .lock()
            .expect("feed poisoned")
            .take()
            .expect("ScriptedFeed::subscribe called twice");
        Ok(Box::new(sub))
    }
}

/// Wraps another `ManifestStore` and rejects every `publish` after the first.
/// Used to prove that an incremental publish failure leaves the source cursor
/// un-acked.
struct FailingAfterFirstPublisher {
    inner: Arc<InMemoryPublisher>,
    publish_count: Mutex<u64>,
}

#[async_trait]
impl ManifestStore for FailingAfterFirstPublisher {
    async fn publish(&self, manifest: &Manifest) -> Result<u64, StoreError> {
        let count = {
            let mut g = self.publish_count.lock().expect("count poisoned");
            *g += 1;
            *g
        };
        if count > 1 {
            return Err(StoreError::Backend("simulated publish failure".into()));
        }
        self.inner.publish(manifest).await
    }
    async fn current(&self) -> Result<Option<Manifest>, StoreError> {
        self.inner.current().await
    }
    async fn watch(
        &self,
    ) -> Result<futures_core::stream::BoxStream<'static, Result<Manifest, StoreError>>, StoreError> {
        self.inner.watch().await
    }
}

fn make_config() -> Config {
    let mut size_per_band = BTreeMap::new();
    size_per_band.insert("hi".to_string(), "4096m".to_string());

    Config {
        service: ServiceMeta {
            name: "test_svc".to_string(),
            ..Default::default()
        },
        source: CfgSource {
            kind: "memory".to_string(),
            dsn: "memory://".to_string(),
            native_crs: CrsCode::new("EPSG:25832"),
            change_feed: None,
            pool: Default::default(),
        },
        artifacts: Artifacts {
            store: ArtifactStore {
                kind: "fs".to_string(),
                endpoint: None,
                bucket: None,
                prefix: None,
                path: None,

                allow_http: false,
            },
            cache: ArtifactCache {
                path: "/tmp".to_string(),
                max_size: "1MiB".to_string(),
                eviction: "lru".to_string(),
                trust_path_hash: false,
            },
        },
        scales: Scales {
            bands: vec![Band {
                name: "hi".to_string(),
                max_denom: 25_000,
            }],
        },
        cells: Cells {
            grid: "regular".to_string(),
            origin: [0.0, 0.0],
            size_per_band,
            extent: Some(Bbox::new(0.0, 0.0, 1.0, 1.0)),
        },
        interfaces: Interfaces::default(),
        tile_matrix_sets: Default::default(),
        reprojection: Default::default(),
        styles: Default::default(),
        layers: vec![Layer {
            name: LayerId::new("roads"),
            title: String::new(),
            abstract_: String::new(),
            kind: "polygon".to_string(),
            scale: None,
            group: None,
            enable_get_feature_info: false,
            bbox: None,
            sources: vec![CfgBinding {
                scale: None,
                band: Some("hi".to_string()),
                from: "public.roads".to_string(),
                geometry_column: "geom".to_string(),
                id_column: Some("gid".to_string()),
                attributes: vec!["attr".to_string()],
            }],
            classes: vec![Class {
                name: "a".to_string(),
                title: String::new(),
                when: Some("attr = 'a'".to_string()),
                style: ClassStyle::Ref {
                    name: "style_a".to_string(),
                },
            }],
            label: None,
        }],
        observability: Default::default(),
        render: Default::default(),
        // 50ms keeps the run loop tight without pegging the test runner.
        compiler: CompilerCfg {
            window: "50ms".to_string(),
            parallel_cells: None,
        },
    }
}

fn make_rows() -> Vec<RowBytes> {
    vec![RowBytes {
        feature_id: 1,
        geometry: wkb_polygon(&[(0.0, 0.0), (5.0, 0.0), (5.0, 5.0), (0.0, 5.0), (0.0, 0.0)]),
        attributes: vec![("attr".to_string(), AttrValue::String("a".to_string()))],
    }]
}

fn cell_00() -> Cell {
    Cell {
        band: ScaleBand::new("hi"),
        x: 0,
        y: 0,
    }
}

fn build_mem_source() -> Arc<MemSource> {
    let mut mem = MemSource::default();
    mem.insert(SourceCollectionId::new("public.roads"), cell_00(), make_rows());
    Arc::new(mem)
}

fn dirty_batch(source_version: &str) -> ChangeBatch {
    ChangeBatch {
        events: vec![ChangeEvent::Insert {
            collection: "public.roads".into(),
            cells: vec![cell_00()],
        }],
        source_version: Some(source_version.into()),
    }
}

fn empty_batch(source_version: &str) -> ChangeBatch {
    // event references a cell outside the configured plan; dirty_cells_for
    // will produce an empty set because the plan only covers public.roads.
    ChangeBatch {
        events: vec![ChangeEvent::Insert {
            collection: "unrelated".into(),
            cells: vec![cell_00()],
        }],
        source_version: Some(source_version.into()),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_acks_latest_source_version_after_publish() {
    let mem = build_mem_source();
    let store = Arc::new(InMemoryStore::new());
    let publisher = Arc::new(InMemoryPublisher::new());

    // bootstrap: precondition with a v=1 manifest so run() skips snapshot.
    let bootstrap = Manifest::new(1, "test_svc", vec![], vec![], None, vec![]);
    publisher.publish(&bootstrap).await.unwrap();

    let acks = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    let feed = Arc::new(ScriptedFeed {
        sub: Mutex::new(Some(ScriptedSub {
            batches: VecDeque::from(vec![Ok(dirty_batch("0/100")), Ok(dirty_batch("0/200"))]),
            acks: acks.clone(),
        })),
    });

    let deps = Deps {
        source: mem.clone() as Arc<dyn mars_source::Source>,
        change_feed: feed as Arc<dyn ChangeFeed>,
        leader_lock: Arc::new(MemLeader::always_grants()) as Arc<dyn mars_source::LeaderLock>,
        store: store.clone() as Arc<dyn ObjectStore>,
        manifest: publisher.clone() as Arc<dyn ManifestStore>,
        metrics: mars_observability::Metrics::new().unwrap(),
    };

    let shutdown = CancellationToken::new();
    let compiler = Compiler::new(deps, make_config());
    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { compiler.run(shutdown).await }
    });

    // give the run loop time to consume both batches, publish, and ack
    tokio::time::sleep(Duration::from_millis(250)).await;
    shutdown.cancel();
    handle.await.unwrap().unwrap();

    let acks = acks.lock().unwrap().clone();
    assert!(!acks.is_empty(), "expected at least one ack after successful publish");
    let last = acks.last().unwrap().as_deref();
    assert_eq!(last, Some("0/200"), "ack should track latest batch source_version");

    let manifest = publisher.current().await.unwrap().unwrap();
    assert!(
        manifest.version >= 2,
        "incremental publish bumps version: {}",
        manifest.version
    );
    assert_eq!(manifest.source_version.as_deref(), Some("0/200"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_does_not_ack_when_publish_fails() {
    let mem = build_mem_source();
    let store = Arc::new(InMemoryStore::new());
    let inner = Arc::new(InMemoryPublisher::new());

    let bootstrap = Manifest::new(1, "test_svc", vec![], vec![], None, vec![]);
    inner.publish(&bootstrap).await.unwrap();

    // wrap so the next publish (the incremental one) errors.
    let publisher = Arc::new(FailingAfterFirstPublisher {
        inner: inner.clone(),
        publish_count: Mutex::new(1),
    });

    let acks = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    let feed = Arc::new(ScriptedFeed {
        sub: Mutex::new(Some(ScriptedSub {
            batches: VecDeque::from(vec![Ok(dirty_batch("0/100"))]),
            acks: acks.clone(),
        })),
    });

    let deps = Deps {
        source: mem.clone() as Arc<dyn mars_source::Source>,
        change_feed: feed as Arc<dyn ChangeFeed>,
        leader_lock: Arc::new(MemLeader::always_grants()) as Arc<dyn mars_source::LeaderLock>,
        store: store.clone() as Arc<dyn ObjectStore>,
        manifest: publisher.clone() as Arc<dyn ManifestStore>,
        metrics: mars_observability::Metrics::new().unwrap(),
    };

    let compiler = Compiler::new(deps, make_config());
    let err = compiler
        .run(CancellationToken::new())
        .await
        .expect_err("publish failure must propagate");
    assert!(matches!(err, mars_compiler::CompilerError::Store(_)));

    assert!(
        acks.lock().unwrap().is_empty(),
        "no ack must be recorded when publish fails"
    );

    // manifest stayed at the bootstrap version.
    let manifest = inner.current().await.unwrap().unwrap();
    assert_eq!(manifest.version, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_acks_empty_window_without_republishing() {
    let mem = build_mem_source();
    let store = Arc::new(InMemoryStore::new());
    let publisher = Arc::new(InMemoryPublisher::new());

    let bootstrap = Manifest::new(1, "test_svc", vec![], vec![], None, vec![]);
    publisher.publish(&bootstrap).await.unwrap();

    let acks = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    let feed = Arc::new(ScriptedFeed {
        sub: Mutex::new(Some(ScriptedSub {
            // batch references a collection outside the plan, so dirty is empty
            batches: VecDeque::from(vec![Ok(empty_batch("0/050"))]),
            acks: acks.clone(),
        })),
    });

    let deps = Deps {
        source: mem.clone() as Arc<dyn mars_source::Source>,
        change_feed: feed as Arc<dyn ChangeFeed>,
        leader_lock: Arc::new(MemLeader::always_grants()) as Arc<dyn mars_source::LeaderLock>,
        store: store.clone() as Arc<dyn ObjectStore>,
        manifest: publisher.clone() as Arc<dyn ManifestStore>,
        metrics: mars_observability::Metrics::new().unwrap(),
    };

    let shutdown = CancellationToken::new();
    let compiler = Compiler::new(deps, make_config());
    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { compiler.run(shutdown).await }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;
    shutdown.cancel();
    handle.await.unwrap().unwrap();

    let acks = acks.lock().unwrap().clone();
    assert_eq!(
        acks.last().map(|s| s.as_deref()),
        Some(Some("0/050")),
        "empty-dirty windows still advance the source cursor"
    );
    assert_eq!(
        publisher.current().await.unwrap().unwrap().version,
        1,
        "no republish when the window produced no dirty cells"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_fails_loudly_when_change_feed_not_implemented() {
    let mem = build_mem_source();
    let store = Arc::new(InMemoryStore::new());
    let publisher = Arc::new(InMemoryPublisher::new());

    let bootstrap = Manifest::new(1, "test_svc", vec![], vec![], None, vec![]);
    publisher.publish(&bootstrap).await.unwrap();

    let deps = Deps {
        source: mem.clone() as Arc<dyn mars_source::Source>,
        // MemSource::subscribe returns NotImplemented
        change_feed: mem.clone() as Arc<dyn mars_source::ChangeFeed>,
        leader_lock: Arc::new(MemLeader::always_grants()) as Arc<dyn mars_source::LeaderLock>,
        store: store.clone() as Arc<dyn ObjectStore>,
        manifest: publisher.clone() as Arc<dyn ManifestStore>,
        metrics: mars_observability::Metrics::new().unwrap(),
    };

    let compiler = Compiler::new(deps, make_config());
    let err = compiler
        .run(CancellationToken::new())
        .await
        .expect_err("NotImplemented feed must be a hard error in service mode");
    assert!(matches!(
        err,
        mars_compiler::CompilerError::Source(SourceError::NotImplemented { .. })
    ));
}
