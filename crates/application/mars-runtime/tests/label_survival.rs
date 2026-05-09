//! runtime label-survival audit (LAZARUS Phase E line 670). orphan label
//! candidates - whose feature_id is absent from the page's geometry section -
//! are kept under `Independent` and dropped under `FollowGeometry`. the
//! runtime is the defensive layer here; the compiler is the primary enforcer
//! but a sidecar carried over from an older epoch could still surface the
//! mismatch on a hot replica.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use mars_render_port::DrawOp;
use mars_style::LabelSurvival;

use common::{FixtureOptions, build_fixture_with};

const ORPHAN_ID: u64 = 9_999_999;

fn orphan_emitted(log: &[DrawOp]) -> bool {
    log.iter().any(|op| match op {
        DrawOp::Label { text, .. } => text.contains("ORPH"),
        _ => false,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn label_survival_follow_geometry_drops_orphan_labels() {
    let opts = FixtureOptions {
        label_survival: LabelSurvival::FollowGeometry,
        orphan_label_feature_ids: vec![ORPHAN_ID],
        ..FixtureOptions::default()
    };
    let fix = build_fixture_with(opts).await;
    let bytes = fix.runtime.render(&fix.render_plan()).await.expect("render");
    assert!(!bytes.is_empty());

    let log = fix.render_log.lock().unwrap();
    assert!(
        !orphan_emitted(&log),
        "FollowGeometry must drop labels whose feature_id is absent from the page geometry"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn label_survival_independent_keeps_orphan_labels() {
    let opts = FixtureOptions {
        label_survival: LabelSurvival::Independent,
        orphan_label_feature_ids: vec![ORPHAN_ID],
        ..FixtureOptions::default()
    };
    let fix = build_fixture_with(opts).await;
    let bytes = fix.runtime.render(&fix.render_plan()).await.expect("render");
    assert!(!bytes.is_empty());

    let log = fix.render_log.lock().unwrap();
    assert!(
        orphan_emitted(&log),
        "Independent must keep orphan labels (compiler-enforced semantics; runtime never filters under this policy)"
    );
}
