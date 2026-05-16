//! Pass 1 of the unified compile pipeline: stream a per-row geometry
//! summary from the source, sort by hilbert key per level, cut page
//! boundaries by accumulated WKB byte length.
//!
//! The output [`PagePlan`] carries per-level page boundaries plus the
//! member feature ids and snapshot-stable row keys. Pass 2 streams the
//! bound collection once via [`mars_source::CompileSession::stream_rows`]
//! and buckets rows into the planned pages by joining each row's
//! [`SourceRowKey`] against the page's `row_keys`. The two passes share a
//! single `REPEATABLE READ` snapshot, so the row set is identical
//! between the planner and the renderer.
//!
//! Memory: each [`PlanRow`] is fixed-size; pass-1 footprint is bounded by
//! `row_count * size_of::<PlanRow>()`. The caller passes a hard plan
//! budget; crossing it yields [`crate::CompilerError::BootstrapPlanTooLarge`]
//! before the planner allocates beyond it.

use std::path::Path;

use futures_util::StreamExt;
use mars_source::{CompileSession, RowSummary, SourceRowKey};
use mars_types::{Bbox, DecimationLevel, HilbertKey, PageId};

use crate::CompilerError;
use crate::decimate::passes_min_size_bbox;
use crate::hilbert::key_from_centroid;
use crate::plan::BindingPlan;
use crate::sidecar_arena::{SidecarArena, SidecarArenaWriter};

/// Per-(binding, level) page plan plus the binding's combined bbox.
#[derive(Debug, Clone, PartialEq)]
pub struct PagePlan {
    /// observed across all rows in pass 1; pass 2 reuses this when
    /// re-keying hydrated rows so the keys agree exactly.
    pub combined_bbox: Bbox,
    /// One entry per configured decimation level.
    pub levels: Vec<LevelPagePlan>,
    /// Total rows seen by pass 1 before per-level filtering.
    pub feature_count_total: u64,
    /// `(feature_id, hilbert_key)` for every unfiltered row pass 1 saw,
    /// stored as a fixed-record on-disk arena. Pass 2 drains it once and
    /// hands the buffer to `encode_sidecar`, which sorts and encodes.
    /// Order is pass-1 stream order.
    pub sidecar_arena: SidecarArena,
}

/// One level's slice of the plan. Pages are emitted in ascending hilbert
/// order; `pages` is empty when no row passes `geometry_min_size_m`.
#[derive(Debug, Clone, PartialEq)]
pub struct LevelPagePlan {
    pub level: DecimationLevel,
    pub pages: Vec<PlannedPage>,
}

/// One planned page. Pass 2 streams the full table once per binding and
/// buckets rows into the planned pages by joining `row_keys` against each
/// row's [`SourceRowKey`]; `feature_ids` is kept parallel for diagnostics
/// and downstream consumers.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedPage {
    /// Assigned by pass 1 in plan order, level-local.
    pub page_id: PageId,
    /// Inclusive `(lo, hi)` hilbert range covered by this page; populates
    /// [`mars_types::LevelMetadata::hilbert_range_table`] verbatim.
    pub hilbert_range: (HilbertKey, HilbertKey),
    /// Member feature ids in pass-1 sort order. Non-unique allowed (a
    /// source row exploded into multiple parts shares one feature id).
    pub feature_ids: Vec<i64>,
    /// Snapshot-stable row identities, parallel to and same length as
    /// `feature_ids`. Pass 2 routes streamed rows to pages via this set;
    /// duplicate `feature_ids` are disambiguated cleanly because each
    /// physical row has its own key.
    pub row_keys: Vec<SourceRowKey>,
    /// Sum of pass-1 byte estimates over the page; diagnostic. Pass 2's
    /// final on-disk page size will differ (decimation reduces line /
    /// polygon bytes, no-op for points).
    pub estimated_bytes: u64,
}

/// fixed-size pass-1 record.
#[derive(Debug, Clone, Copy)]
struct PlanRow {
    feature_id: i64,
    bbox: [f32; 4],
    geom_byte_length: u32,
    row_key: SourceRowKey,
    hilbert_key: HilbertKey,
}

/// Drain a [`CompileSession`] geometry summary stream into a per-binding
/// page plan. The session must be freshly opened (no other stream alive).
pub async fn compute_page_plan(
    session: &mut (dyn CompileSession + '_),
    binding: &BindingPlan,
    plan_budget_bytes: u64,
    scratch_dir: &Path,
) -> Result<PagePlan, CompilerError> {
    let row_size = std::mem::size_of::<PlanRow>() as u64;
    let max_rows = plan_budget_bytes
        .checked_div(row_size)
        .map_or(usize::MAX, |n| usize::try_from(n).unwrap_or(usize::MAX));

    let started = std::time::Instant::now();
    tracing::info!(
        target: "mars_compiler::compile",
        binding = %binding.binding_id,
        "compile.plan.start",
    );

    let mut rows: Vec<PlanRow> = Vec::new();
    let mut bbox_acc = BboxAcc::default();
    let mut feature_count_total: u64 = 0;
    {
        let mut stream = session.stream_geometry_summary().await?;
        while let Some(item) = stream.next().await {
            let s: RowSummary = item?;
            if rows.len() >= max_rows {
                return Err(CompilerError::BootstrapPlanTooLarge {
                    binding: binding.binding_id.as_str().to_string(),
                    observed_rows: feature_count_total.saturating_add(1),
                    budget_bytes: plan_budget_bytes,
                });
            }
            bbox_acc.fold(s.bbox);
            rows.push(PlanRow {
                feature_id: s.feature_id,
                bbox: s.bbox,
                geom_byte_length: s.geom_byte_length,
                row_key: s.row_key,
                hilbert_key: HilbertKey::min(),
            });
            feature_count_total = feature_count_total.saturating_add(1);
        }
    }
    let combined_bbox = bbox_acc.into_bbox();

    // assign hilbert keys against the combined bbox - matches the rebuild
    // path's bbox-midpoint formulation (rebuild.rs key derivation).
    for r in rows.iter_mut() {
        let cx = (f64::from(r.bbox[0]) + f64::from(r.bbox[2])) / 2.0;
        let cy = (f64::from(r.bbox[1]) + f64::from(r.bbox[3])) / 2.0;
        r.hilbert_key = key_from_centroid(cx, cy, combined_bbox);
    }

    // sidecar entries: (user_id, hilbert_key) for every unfiltered row.
    // postgres rejects negative ids upstream, so the i64 -> u64 cast preserves
    // value. arena is on-disk; pass 2 drains once.
    let mut arena_writer = SidecarArenaWriter::new(scratch_dir)?;
    for r in &rows {
        arena_writer.push(r.feature_id as u64, r.hilbert_key)?;
    }
    let sidecar_arena = arena_writer.finish()?;

    let mut levels: Vec<LevelPagePlan> = Vec::with_capacity(binding.levels.len());
    for level in &binding.levels {
        let mut level_rows: Vec<PlanRow> = rows
            .iter()
            .filter(|r| passes_min_size_bbox(r.bbox, level.geometry_min_size_m))
            .copied()
            .collect();
        // (hilbert_key, feature_id, row_key). row_key is unique within the
        // snapshot, so this triple is strictly sortable; pass-2 page sort
        // shares the same prefix `(hilbert_key, feature_id)`. boundary-edge
        // ties on the prefix shuffle within their page (today they shuffle
        // across runs anyway - pass-1 md5 vs pass-2 BLAKE3 disagree); the
        // page assignment itself stays deterministic.
        level_rows.sort_by(|a, b| {
            a.hilbert_key
                .cmp(&b.hilbert_key)
                .then_with(|| a.feature_id.cmp(&b.feature_id))
                .then_with(|| a.row_key.cmp(&b.row_key))
        });
        let pages = sweep_pages(&level_rows, binding.page_size_target_bytes);
        levels.push(LevelPagePlan {
            level: level.level,
            pages,
        });
    }

    let plan = PagePlan {
        combined_bbox,
        levels,
        feature_count_total,
        sidecar_arena,
    };
    let total_pages: usize = plan.levels.iter().map(|l| l.pages.len()).sum();
    tracing::info!(
        target: "mars_compiler::compile",
        binding = %binding.binding_id,
        elapsed_ms = started.elapsed().as_millis() as u64,
        feature_count_total = plan.feature_count_total,
        levels = plan.levels.len(),
        pages = total_pages,
        bbox_min_x = plan.combined_bbox.min_x,
        bbox_min_y = plan.combined_bbox.min_y,
        bbox_max_x = plan.combined_bbox.max_x,
        bbox_max_y = plan.combined_bbox.max_y,
        "compile.plan.end",
    );
    Ok(plan)
}

/// Sweep an already-sorted level slice into pages whose accumulated WKB
/// byte estimate stays at or below `target_bytes`.
fn sweep_pages(rows: &[PlanRow], target_bytes: u64) -> Vec<PlannedPage> {
    let mut pages: Vec<PlannedPage> = Vec::new();
    if rows.is_empty() {
        return pages;
    }
    let mut next_id: u64 = 0;
    let mut current_ids: Vec<i64> = Vec::new();
    let mut current_keys: Vec<SourceRowKey> = Vec::new();
    let mut current_lo = rows[0].hilbert_key;
    let mut current_hi = rows[0].hilbert_key;
    let mut current_bytes: u64 = 0;
    // 64 mirrors the pre-existing per-row "+64" overhead estimate in
    // snapshot.rs `estimate_row_size`; pass 2's flush_page also pays it.
    const PER_ROW_OVERHEAD: u64 = 64;

    for r in rows {
        let est = u64::from(r.geom_byte_length).saturating_add(PER_ROW_OVERHEAD);
        if !current_ids.is_empty() && current_bytes.saturating_add(est) > target_bytes {
            pages.push(PlannedPage {
                page_id: PageId::new(next_id),
                hilbert_range: (current_lo, current_hi),
                feature_ids: std::mem::take(&mut current_ids),
                row_keys: std::mem::take(&mut current_keys),
                estimated_bytes: current_bytes,
            });
            next_id = next_id.saturating_add(1);
            current_lo = r.hilbert_key;
            current_bytes = 0;
        }
        if current_ids.is_empty() {
            current_lo = r.hilbert_key;
        }
        current_hi = r.hilbert_key;
        current_bytes = current_bytes.saturating_add(est);
        current_ids.push(r.feature_id);
        current_keys.push(r.row_key);
    }
    if !current_ids.is_empty() {
        pages.push(PlannedPage {
            page_id: PageId::new(next_id),
            hilbert_range: (current_lo, current_hi),
            feature_ids: current_ids,
            row_keys: current_keys,
            estimated_bytes: current_bytes,
        });
    }
    pages
}

#[derive(Default)]
struct BboxAcc {
    seen: bool,
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl BboxAcc {
    fn fold(&mut self, bb: [f32; 4]) {
        let lx = f64::from(bb[0]);
        let ly = f64::from(bb[1]);
        let hx = f64::from(bb[2]);
        let hy = f64::from(bb[3]);
        if !self.seen {
            self.min_x = lx;
            self.min_y = ly;
            self.max_x = hx;
            self.max_y = hy;
            self.seen = true;
            return;
        }
        if lx < self.min_x {
            self.min_x = lx;
        }
        if ly < self.min_y {
            self.min_y = ly;
        }
        if hx > self.max_x {
            self.max_x = hx;
        }
        if hy > self.max_y {
            self.max_y = hy;
        }
    }

    fn into_bbox(self) -> Bbox {
        if !self.seen {
            return Bbox::new(0.0, 0.0, 0.0, 0.0);
        }
        Bbox::new(self.min_x, self.min_y, self.max_x, self.max_y)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
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
}
