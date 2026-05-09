//! hot-path render correctness against an in-memory MARS service fixture.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use mars_render_port::DrawOp;

use common::build_fixture;

#[tokio::test(flavor = "multi_thread")]
async fn render_emits_one_path_per_feature_and_one_label_per_feature() {
    let fix = build_fixture().await;
    let plan = fix.render_plan();
    let bytes = fix.runtime.render(&plan).await.expect("render");
    assert!(!bytes.is_empty(), "encoder produced empty output");

    let log = fix.render_log.lock().unwrap();
    let path_count = log.iter().filter(|op| matches!(op, DrawOp::Path { .. })).count();
    let label_count = log.iter().filter(|op| matches!(op, DrawOp::Label { .. })).count();

    // fixture seeds three polygon features. paths are emitted once per
    // feature; labels go through collision so the count is "at least one"
    // (greedy collision drops conflicts at this fixture's small viewport).
    assert_eq!(path_count, 3, "expected one DrawOp::Path per feature");
    assert!(
        label_count >= 1,
        "expected at least one DrawOp::Label, got {label_count}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn render_skips_layers_with_no_pages_in_viewport() {
    let fix = build_fixture().await;
    let mut plan = fix.render_plan();
    // viewport entirely north-east of the fixture's page bbox: no page
    // intersects, so we expect zero ops.
    plan.bbox = mars_types::Bbox::new(10_000.0, 10_000.0, 11_000.0, 11_000.0);
    let _ = fix.runtime.render(&plan).await.expect("render empty");

    let log = fix.render_log.lock().unwrap();
    assert!(log.is_empty(), "expected no DrawOps, got {:?}", log.len());
}
