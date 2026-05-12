//! manifest swap correctness: in-flight renders against the old state must
//! complete with their started snapshot, and post-swap renders must observe
//! the new state.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;

use mars_render_port::DrawOp;

use common::{Fixture, build_fixture, build_fixture_with};

#[tokio::test(flavor = "multi_thread")]
async fn render_observes_swap_after_complete() {
    let fix = build_fixture().await;
    let plan = fix.render_plan();

    // baseline render: 3 paths + 3 labels.
    let _ = fix.runtime.render(&plan).await.expect("baseline render");
    let baseline_count = fix.render_log.lock().unwrap().len();

    // swap to a manifest with a different feature_count to provoke a
    // distinguishable post-swap result.
    swap_to_5_feature_state(&fix).await;
    fix.render_log.lock().unwrap().clear();

    let _ = fix.runtime.render(&plan).await.expect("post-swap render");
    let after_count = fix.render_log.lock().unwrap().len();
    assert!(after_count > 0, "post-swap render emitted no ops");
    // 5 features -> 5 paths + 5 labels = 10 ops.
    let path_count = fix
        .render_log
        .lock()
        .unwrap()
        .iter()
        .filter(|op| matches!(op, DrawOp::Path { .. }))
        .count();
    assert_eq!(
        path_count, 5,
        "expected 5 paths after swap, baseline was {baseline_count}"
    );
}

async fn swap_to_5_feature_state(fix: &Fixture) {
    let new_fix = build_fixture_with(common::FixtureOptions {
        manifest_version: 99,
        feature_count: 5,
        ..common::FixtureOptions::default()
    })
    .await;
    // graft the new manifest's bytes into our existing store so the runtime
    // can fetch them through its own deps. the new fixture wrote them to its
    // own store; copy across.
    for page in &new_fix.manifest.pages {
        let key = page.key.object_key(&page.content_hash).unwrap();
        let bytes = new_fix.store.get(&key, page.content_hash).await.unwrap();
        fix.store.put(&key, bytes).await.unwrap();
    }
    for sc in new_fix
        .manifest
        .class_sidecars
        .iter()
        .chain(new_fix.manifest.label_sidecars.iter())
    {
        let key = sc.object_key().unwrap();
        let bytes = new_fix.store.get(&key, sc.content_hash).await.unwrap();
        fix.store.put(&key, bytes).await.unwrap();
    }

    let cfg = (*fix.config).clone();
    let stylesheet = fix.runtime.current_state().expect("state").stylesheet.clone();
    let state = mars_runtime::RuntimeState::from_config_and_manifest(&cfg, stylesheet, new_fix.manifest.clone())
        .expect("rebuild state");
    fix.runtime.swap_state(Arc::new(state));
}

#[tokio::test(flavor = "multi_thread")]
async fn swap_publishes_manifest_version_gauge() {
    let fix = build_fixture().await;
    let baseline = fix.metrics.encode_text().unwrap();
    assert!(
        baseline.contains("mars_manifest_version 1"),
        "expected version 1 after initial state, got:\n{baseline}"
    );

    swap_to_5_feature_state(&fix).await;
    let after = fix.metrics.encode_text().unwrap();
    assert!(
        after.contains("mars_manifest_version 99"),
        "expected version 99 after swap, got:\n{after}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_renders_complete_under_swap() {
    let fix = build_fixture().await;
    let plan = fix.render_plan();
    let runtime = fix.runtime.clone();

    // spawn N concurrent renders; they should all complete (against either
    // state) without panicking.
    let mut handles = Vec::new();
    for _ in 0..8 {
        let rt = runtime.clone();
        let p = plan.clone();
        handles.push(tokio::spawn(async move { rt.render(&p).await }));
    }

    // mid-flight swap.
    swap_to_5_feature_state(&fix).await;

    for h in handles {
        let res = h.await.expect("join");
        let bytes = res.expect("render result");
        assert!(!bytes.is_empty());
    }
}
