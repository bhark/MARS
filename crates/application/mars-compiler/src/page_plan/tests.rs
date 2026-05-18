#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use async_trait::async_trait;
use futures_core::stream::BoxStream;
use futures_util::stream;
use mars_config::{MissingPagePolicy, SimplifierKind, SourceId};
use mars_source::{RowBytes, SourceError};
use mars_types::{BindingId, CrsCode};

use crate::plan::LevelPlan;

/// In-memory CompileSession fake. Yields a fixed set of summaries; the
/// `feature_ids` half of the contract is unused at this layer (pass 2
/// is exercised in `rebuild` tests).
struct FakeSession {
    summaries: Vec<RowSummary>,
}

#[async_trait]
impl CompileSession for FakeSession {
    async fn stream_geometry_summary<'a>(
        &'a mut self,
    ) -> Result<BoxStream<'a, Result<RowSummary, SourceError>>, SourceError> {
        let drained = std::mem::take(&mut self.summaries);
        Ok(Box::pin(stream::iter(drained.into_iter().map(Ok))))
    }
    async fn stream_rows<'a>(&'a mut self) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        Ok(Box::pin(stream::empty()))
    }
    async fn commit(self: Box<Self>) -> Result<(), SourceError> {
        Ok(())
    }
    async fn rollback(self: Box<Self>) -> Result<(), SourceError> {
        Ok(())
    }
}

fn binding_plan(page_target: u64, levels: Vec<LevelPlan>) -> BindingPlan {
    BindingPlan {
        binding_id: BindingId::try_new("planned").unwrap(),
        source_id: SourceId::new("default"),
        source_table: "planned".into(),
        filter: None,
        geometry_field: "geom".into(),
        id_field: Some("id".into()),
        attributes: vec![],
        native_crs: CrsCode::new("EPSG:25832"),
        levels,
        page_size_target_bytes: page_target,
        sidecar_size_warn_bytes: u64::MAX,
        reconcile_every_cycles: 24,
        simplifier: SimplifierKind::Naive,
        missing_page_policy: MissingPagePolicy::Truncate,
        dsn: None,
    }
}

fn level(min_size_m: f64) -> LevelPlan {
    LevelPlan {
        level: DecimationLevel::new(0),
        vertex_tolerance_m: 0.0,
        geometry_min_size_m: min_size_m,
        label_min_priority: 0,
    }
}

fn summary(id: i64, x: f32, y: f32, len: u32, key_seed: u64) -> RowSummary {
    let mut k = [0u8; 16];
    k[..8].copy_from_slice(&key_seed.to_le_bytes());
    RowSummary {
        feature_id: id,
        // zero-area bbox at (x, y).
        bbox: [x, y, x, y],
        geom_byte_length: len,
        row_key: SourceRowKey::from_bytes(k),
    }
}

#[tokio::test]
async fn empty_source_yields_empty_plan() {
    let mut sess: Box<dyn CompileSession> = Box::new(FakeSession { summaries: vec![] });
    let bp = binding_plan(1024, vec![level(0.0)]);
    let plan = compute_page_plan(sess.as_mut(), &bp, 8 * 1024 * 1024, std::env::temp_dir().as_path())
        .await
        .unwrap();
    assert_eq!(plan.feature_count_total, 0);
    assert_eq!(plan.levels.len(), 1);
    assert!(plan.levels[0].pages.is_empty());
}

#[tokio::test]
async fn single_page_when_under_target() {
    let summaries: Vec<RowSummary> = (0..10)
        .map(|i| summary(i, i as f32 * 10.0, 0.0, 32, i as u64))
        .collect();
    let mut sess: Box<dyn CompileSession> = Box::new(FakeSession {
        summaries: summaries.clone(),
    });
    let bp = binding_plan(64 * 1024, vec![level(0.0)]);
    let plan = compute_page_plan(sess.as_mut(), &bp, 8 * 1024 * 1024, std::env::temp_dir().as_path())
        .await
        .unwrap();
    assert_eq!(plan.feature_count_total, 10);
    assert_eq!(plan.levels.len(), 1);
    assert_eq!(plan.levels[0].pages.len(), 1);
    assert_eq!(plan.levels[0].pages[0].feature_ids.len(), 10);
}

#[tokio::test]
async fn small_target_splits_into_many_pages() {
    let summaries: Vec<RowSummary> = (0..1_000).map(|i| summary(i, i as f32, 0.0, 64, i as u64)).collect();
    let mut sess: Box<dyn CompileSession> = Box::new(FakeSession { summaries });
    // (64 + 64) bytes per row = 128; with 256 byte target, 2 rows/page.
    let bp = binding_plan(256, vec![level(0.0)]);
    let plan = compute_page_plan(sess.as_mut(), &bp, 64 * 1024 * 1024, std::env::temp_dir().as_path())
        .await
        .unwrap();
    assert_eq!(plan.feature_count_total, 1_000);
    let pages = &plan.levels[0].pages;
    assert!(pages.len() > 100);
    let total_ids: usize = pages.iter().map(|p| p.feature_ids.len()).sum();
    assert_eq!(total_ids, 1_000);
    // ranges are non-overlapping ascending.
    for w in pages.windows(2) {
        assert!(w[0].hilbert_range.1 <= w[1].hilbert_range.0);
    }
}

#[tokio::test]
async fn level_filter_drops_undersize_rows() {
    // mix three sub-1m bbox rows with three 100m bboxes.
    let summaries = vec![
        RowSummary {
            feature_id: 1,
            bbox: [0.0, 0.0, 0.5, 0.5],
            geom_byte_length: 32,
            row_key: SourceRowKey::from_bytes([1; 16]),
        },
        RowSummary {
            feature_id: 2,
            bbox: [10.0, 10.0, 110.0, 110.0],
            geom_byte_length: 32,
            row_key: SourceRowKey::from_bytes([2; 16]),
        },
        RowSummary {
            feature_id: 3,
            bbox: [0.1, 0.1, 0.2, 0.2],
            geom_byte_length: 32,
            row_key: SourceRowKey::from_bytes([3; 16]),
        },
        RowSummary {
            feature_id: 4,
            bbox: [200.0, 200.0, 300.0, 300.0],
            geom_byte_length: 32,
            row_key: SourceRowKey::from_bytes([4; 16]),
        },
        RowSummary {
            feature_id: 5,
            bbox: [0.3, 0.3, 0.4, 0.4],
            geom_byte_length: 32,
            row_key: SourceRowKey::from_bytes([5; 16]),
        },
        RowSummary {
            feature_id: 6,
            bbox: [400.0, 400.0, 500.0, 500.0],
            geom_byte_length: 32,
            row_key: SourceRowKey::from_bytes([6; 16]),
        },
    ];
    let mut sess: Box<dyn CompileSession> = Box::new(FakeSession { summaries });
    let bp = binding_plan(
        16 * 1024,
        vec![
            level(0.0),  // keep all
            level(50.0), // keep only the three large bboxes
        ],
    );
    let plan = compute_page_plan(sess.as_mut(), &bp, 8 * 1024 * 1024, std::env::temp_dir().as_path())
        .await
        .unwrap();
    let l0_total: usize = plan.levels[0].pages.iter().map(|p| p.feature_ids.len()).sum();
    let l1_total: usize = plan.levels[1].pages.iter().map(|p| p.feature_ids.len()).sum();
    assert_eq!(l0_total, 6);
    assert_eq!(l1_total, 3);
}

#[tokio::test]
async fn plan_budget_overrun_yields_named_error() {
    // 200 rows; budget allows ~3 rows.
    let summaries: Vec<RowSummary> = (0..200).map(|i| summary(i, i as f32, 0.0, 16, i as u64)).collect();
    let mut sess: Box<dyn CompileSession> = Box::new(FakeSession { summaries });
    let bp = binding_plan(16 * 1024, vec![level(0.0)]);
    let row_size = std::mem::size_of::<PlanRow>() as u64;
    let budget = row_size * 3;
    let err = compute_page_plan(sess.as_mut(), &bp, budget, std::env::temp_dir().as_path())
        .await
        .unwrap_err();
    match err {
        CompilerError::BootstrapPlanTooLarge {
            binding,
            observed_rows,
            budget_bytes,
        } => {
            assert_eq!(binding, "planned");
            assert!(observed_rows > 3);
            assert_eq!(budget_bytes, budget);
        }
        other => panic!("expected BootstrapPlanTooLarge, got {other:?}"),
    }
}
