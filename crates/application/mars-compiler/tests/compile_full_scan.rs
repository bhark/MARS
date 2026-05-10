//! Coverage for the single-scan rewrite of `rebuild_binding_from_plan`.
//!
//! Drives the renderer via a hand-rolled `CompileSession` so the tests can
//! control row order, missing rows, and emit pressure that
//! `FullScanCompileSession`'s `Source` indirection hides.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use futures_util::stream;
use mars_compiler::page_plan::{LevelPagePlan, PagePlan, PlannedPage};
use mars_compiler::plan::{BindingPlan, BootstrapPlan, LevelPlan};
use mars_compiler::render::rebuild_binding_from_plan;
use mars_compiler::{CompilerError, Deps};
use mars_observability::Metrics;
use mars_source::{
    AttrValue, ChangeFeed, ChangeSubscription, CompileSession, LeaderLock, LeaderLockGuard, RowBytes, RowSummary,
    Source, SourceBinding as PortBinding, SourceError, SourceRowKey,
};
use mars_store::ManifestStore;
use mars_store::mem::{InMemoryPublisher, InMemoryStore};
use mars_types::{Bbox, BindingId, CrsCode, DecimationLevel, HilbertKey, PageId};

// 21-byte little-endian point WKB.
fn point_wkb(x: f64, y: f64) -> Bytes {
    let mut v = Vec::with_capacity(21);
    v.push(1);
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&x.to_le_bytes());
    v.extend_from_slice(&y.to_le_bytes());
    Bytes::from(v)
}

fn row(id: u64, x: f64, y: f64, key_seed: u64) -> RowBytes {
    let mut k = [0u8; 16];
    k[..8].copy_from_slice(&key_seed.to_le_bytes());
    RowBytes {
        feature_id: id,
        geometry: point_wkb(x, y),
        attributes: vec![("name".into(), AttrValue::String(format!("p{id}")))],
        row_key: SourceRowKey::from_bytes(k),
    }
}

#[derive(Default)]
struct UnusedSource;
#[async_trait]
impl Source for UnusedSource {
    async fn fetch_full_table_streaming<'a>(
        &'a self,
        _binding: &'a PortBinding,
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        Ok(Box::pin(stream::empty()))
    }
    async fn fetch_by_feature_ids<'a>(
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
            what: "compile_full_scan test source",
        })
    }
}

#[derive(Default)]
struct NopFeed;
#[async_trait]
impl ChangeFeed for NopFeed {
    async fn subscribe(&self) -> Result<Box<dyn ChangeSubscription>, SourceError> {
        Err(SourceError::NotImplemented { what: "test feed" })
    }
}

#[derive(Default)]
struct NopLock;
#[async_trait]
impl LeaderLock for NopLock {
    async fn try_acquire(&self, _key: i64) -> Result<Option<Box<dyn LeaderLockGuard>>, SourceError> {
        Err(SourceError::NotImplemented { what: "test lock" })
    }
}

fn make_deps() -> Deps {
    Deps {
        source: Arc::new(UnusedSource),
        change_feed: Arc::new(NopFeed),
        leader_lock: Arc::new(NopLock),
        store: Arc::new(InMemoryStore::new()),
        manifest: Arc::new(InMemoryPublisher::new()) as Arc<dyn ManifestStore>,
        metrics: Metrics::new().unwrap(),
    }
}

fn binding_plan() -> BindingPlan {
    BindingPlan {
        binding_id: BindingId::try_new("points").unwrap(),
        source_table: "public.points".into(),
        geometry_column: "geom".into(),
        id_column: Some("id".into()),
        attributes: vec!["name".into()],
        native_crs: CrsCode::new("EPSG:25832"),
        levels: vec![LevelPlan {
            level: DecimationLevel::new(0),
            vertex_tolerance_m: 0.0,
            geometry_min_size_m: 0.0,
            label_min_priority: 0,
        }],
        page_size_target_bytes: 64 * 1024,
        sidecar_size_warn_bytes: u64::MAX,
        reconcile_every_cycles: u32::MAX,
        simplifier: mars_config::SimplifierKind::Naive,
    }
}

/// One-level page plan covering `rows`. Single page so the test focuses on
/// per-row routing without page boundary noise.
fn page_plan_for(rows: &[RowBytes]) -> PagePlan {
    let row_keys: Vec<SourceRowKey> = rows.iter().map(|r| r.row_key).collect();
    let feature_ids: Vec<i64> = rows.iter().map(|r| r.feature_id as i64).collect();
    let page = PlannedPage {
        page_id: PageId::new(0),
        hilbert_range: (HilbertKey::min(), HilbertKey::max()),
        feature_ids,
        row_keys,
        estimated_bytes: rows.iter().map(|r| r.geometry.len() as u64 + 64).sum(),
    };
    let mut writer = mars_compiler::sidecar_arena::SidecarArenaWriter::new(std::env::temp_dir().as_path()).unwrap();
    for r in rows {
        writer.push(r.feature_id, HilbertKey::min()).unwrap();
    }
    let sidecar_arena = writer.finish().unwrap();
    PagePlan {
        combined_bbox: Bbox::new(-1000.0, -1000.0, 1000.0, 1000.0),
        levels: vec![LevelPagePlan {
            level: DecimationLevel::new(0),
            pages: vec![page],
        }],
        feature_count_total: rows.len() as u64,
        sidecar_arena,
    }
}

/// Direct `CompileSession` test fake. Yields the supplied row set as the
/// pass-2 stream; pass-1 is unused because the page plan is hand-built.
struct ScriptedSession {
    rows: Vec<RowBytes>,
}

#[async_trait]
impl CompileSession for ScriptedSession {
    async fn fetch_geometry_summary<'a>(
        &'a mut self,
    ) -> Result<BoxStream<'a, Result<RowSummary, SourceError>>, SourceError> {
        // pass-1 is not exercised by these tests; the page plan is supplied
        // by the test harness directly.
        Ok(Box::pin(stream::empty()))
    }

    async fn fetch_full_table_streaming<'a>(
        &'a mut self,
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        let drained = std::mem::take(&mut self.rows);
        Ok(Box::pin(stream::iter(drained.into_iter().map(Ok))))
    }

    async fn commit(self: Box<Self>) -> Result<(), SourceError> {
        Ok(())
    }
    async fn rollback(self: Box<Self>) -> Result<(), SourceError> {
        Ok(())
    }
}

/// Two physical rows sharing one feature_id but distinct geometry land on
/// the page distinctly because routing is keyed by `SourceRowKey`, not
/// `feature_id`.
#[tokio::test]
async fn duplicate_feature_id_routes_distinct_rows() {
    let rows = vec![
        row(7, 10.0, 10.0, 0xAAAA_AAAA_AAAA_AAAA),
        row(7, 20.0, 20.0, 0xBBBB_BBBB_BBBB_BBBB),
        row(7, 30.0, 30.0, 0xCCCC_CCCC_CCCC_CCCC),
    ];
    let deps = make_deps();
    let bp = binding_plan();
    let page_plan = page_plan_for(&rows);
    let plan = BootstrapPlan {
        bindings: vec![bp.clone()],
        layers: vec![],
    };
    let mut session = ScriptedSession { rows };

    let out = rebuild_binding_from_plan(
        &deps,
        &plan,
        &bp,
        &page_plan,
        &mut session,
        4 * 1024 * 1024,
        4 * 1024 * 1024,
        &std::env::temp_dir(),
        256,
        &mars_compiler::memory_governor::MemoryGovernor::new(u64::MAX),
    )
    .await
    .unwrap();
    assert_eq!(out.pages.len(), 1);
    assert_eq!(out.pages[0].feature_count, 3);
}

/// Stream produces a row whose key is in no plan target -- routes to no
/// page, drops silently. Then yields all expected rows so the short-stream
/// invariant doesn't trip.
#[tokio::test]
async fn unrelated_rows_in_stream_are_skipped() {
    let routed = row(1, 0.0, 0.0, 0x1111_1111_1111_1111);
    let unrelated = row(99, 5.0, 5.0, 0x9999_9999_9999_9999);

    let deps = make_deps();
    let bp = binding_plan();
    let page_plan = page_plan_for(std::slice::from_ref(&routed));
    let plan = BootstrapPlan {
        bindings: vec![bp.clone()],
        layers: vec![],
    };
    let mut session = ScriptedSession {
        rows: vec![unrelated, routed],
    };

    let out = rebuild_binding_from_plan(
        &deps,
        &plan,
        &bp,
        &page_plan,
        &mut session,
        4 * 1024 * 1024,
        4 * 1024 * 1024,
        &std::env::temp_dir(),
        256,
        &mars_compiler::memory_governor::MemoryGovernor::new(u64::MAX),
    )
    .await
    .unwrap();
    assert_eq!(out.pages.len(), 1);
    assert_eq!(out.pages[0].feature_count, 1);
}

/// Stream returns fewer rows than the plan says are members. The renderer
/// must surface this as a typed invariant violation rather than silently
/// emit an under-populated page.
#[tokio::test]
async fn short_stream_trips_invariant_violation() {
    let r1 = row(1, 0.0, 0.0, 0x1111_1111_1111_1111);
    let r2 = row(2, 1.0, 1.0, 0x2222_2222_2222_2222);

    let deps = make_deps();
    let bp = binding_plan();
    let page_plan = page_plan_for(&[r1.clone(), r2]);
    let plan = BootstrapPlan {
        bindings: vec![bp.clone()],
        layers: vec![],
    };
    // emit r1 only.
    let mut session = ScriptedSession { rows: vec![r1] };

    let err = rebuild_binding_from_plan(
        &deps,
        &plan,
        &bp,
        &page_plan,
        &mut session,
        4 * 1024 * 1024,
        4 * 1024 * 1024,
        &std::env::temp_dir(),
        256,
        &mars_compiler::memory_governor::MemoryGovernor::new(u64::MAX),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, CompilerError::InvariantViolation { what } if what.contains("fewer rows")),
        "got {err:?}"
    );
}

/// Tight in-flight budget triggers the disk-spill fallback rather than
/// erroring. The page still emits correctly: the spill round-trip
/// preserves rows byte-for-byte and `flush_one_page` re-sorts before
/// encoding so on-disk output is identical to the in-memory path.
#[tokio::test]
async fn budget_overrun_spills_and_completes() {
    let r1 = row(1, 0.0, 0.0, 0x1111_1111_1111_1111);
    let r2 = row(2, 1.0, 1.0, 0x2222_2222_2222_2222);

    let deps = make_deps();
    let bp = binding_plan();
    let page_plan = page_plan_for(&[r1.clone(), r2.clone()]);
    let plan = BootstrapPlan {
        bindings: vec![bp.clone()],
        layers: vec![],
    };
    let mut session = ScriptedSession { rows: vec![r1, r2] };

    let out = rebuild_binding_from_plan(
        &deps,
        &plan,
        &bp,
        &page_plan,
        &mut session,
        4 * 1024 * 1024,
        1, // 1-byte trigger forces spill on the very first row
        &std::env::temp_dir(),
        256,
        &mars_compiler::memory_governor::MemoryGovernor::new(u64::MAX),
    )
    .await
    .unwrap();
    assert_eq!(out.pages.len(), 1);
    assert_eq!(out.pages[0].feature_count, 2);
}

/// Equivalence invariant: the spill fallback must produce byte-identical
/// page artifacts to the in-memory path. Triggers spill on the very first
/// row so every subsequent row is written via the spill-append path; the
/// drain-and-flush at page completion must reconstruct the same KeyedRow
/// sequence the in-memory path would have held.
#[tokio::test]
async fn spilled_path_emits_identical_artifacts_to_in_memory() {
    let rows: Vec<RowBytes> = (0u64..8)
        .map(|i| row(i + 1, i as f64, (i as f64) * 0.5, 0x1000_0000_0000_0000 ^ (i + 1)))
        .collect();
    let bp = binding_plan();
    let page_plan = page_plan_for(&rows);
    let plan = BootstrapPlan {
        bindings: vec![bp.clone()],
        layers: vec![],
    };

    // baseline: in-memory only.
    let deps_mem = make_deps();
    let mut session_mem = ScriptedSession { rows: rows.clone() };
    let out_mem = rebuild_binding_from_plan(
        &deps_mem,
        &plan,
        &bp,
        &page_plan,
        &mut session_mem,
        4 * 1024 * 1024,
        u64::MAX,
        &std::env::temp_dir(),
        256,
        &mars_compiler::memory_governor::MemoryGovernor::new(u64::MAX),
    )
    .await
    .unwrap();

    // forced spill on first row.
    let deps_spill = make_deps();
    let mut session_spill = ScriptedSession { rows: rows.clone() };
    let out_spill = rebuild_binding_from_plan(
        &deps_spill,
        &plan,
        &bp,
        &page_plan,
        &mut session_spill,
        4 * 1024 * 1024,
        1,
        &std::env::temp_dir(),
        256,
        &mars_compiler::memory_governor::MemoryGovernor::new(u64::MAX),
    )
    .await
    .unwrap();

    assert_eq!(out_mem.pages.len(), out_spill.pages.len());
    for (m, s) in out_mem.pages.iter().zip(out_spill.pages.iter()) {
        assert_eq!(
            m.content_hash,
            s.content_hash,
            "spill path produced a different artifact than the in-memory path: \
             page_id={} feature_count mem={} spill={}",
            m.key.page_id.get(),
            m.feature_count,
            s.feature_count,
        );
        assert_eq!(m.spatial_bbox, s.spatial_bbox);
        assert_eq!(m.feature_count, s.feature_count);
        assert_eq!(m.hilbert_range, s.hilbert_range);
    }
}
