//! GetFeatureInfo: pixel-space click resolves to feature attrs.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use mars_artifact::AttrValue;

use common::build_fixture;

#[tokio::test(flavor = "multi_thread")]
async fn gfi_returns_feature_attrs_for_known_pixel_click() {
    let fix = build_fixture().await;
    let plan = fix.render_plan();
    // fixture features sit at (10*i, 10*i) -> (10*i+10, 10*i+10) in world
    // units; viewport is 100x100 mapped to 64x64 px. feature 1000 sits at
    // world (0,0)..(10,10). pixel-space top-left, so y=0..10 world -> y=64
    // .. y≈57 px (pixel y is flipped). a click near (3, 60) px lands
    // squarely inside feature 1000.
    let hits = fix.runtime.get_feature_info(&plan, (3, 60)).await.expect("gfi");
    assert!(!hits.is_empty(), "expected at least one hit, got none");
    let first = hits.iter().find(|h| h.user_id == 1000).expect("feature 1000");
    let name_attr = first.attrs.iter().find(|(k, _)| k == "name").expect("name attr");
    match &name_attr.1 {
        AttrValue::String(s) => assert_eq!(s, "feat-1000"),
        other => panic!("expected string attr, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn gfi_returns_empty_when_layer_lacks_get_feature_info_permission() {
    let fix = build_fixture().await;
    // mutate the in-memory state's config to disable GFI on the layer; the
    // runtime should then refuse to surface attributes for that layer even
    // though the page hits the click.
    let state = fix.runtime.current_state().expect("state");
    let mut new_cfg: mars_config::Config = state.config.as_ref().unwrap().as_ref().clone();
    if let Some(layer) = new_cfg.layers.iter_mut().find(|l| l.name == fix.layer_id) {
        layer.wms.enable_get_feature_info = false;
    }
    let new_state = mars_runtime::RuntimeState::from_config_and_manifest(
        &new_cfg,
        state.stylesheet.clone(),
        // bump version to satisfy the swap_state monotonic gate; runtime does
        // not enforce monotonicity on direct swap_state calls but the public
        // API is a fair shape to exercise.
        mars_types::Manifest {
            version: state.manifest.version + 1,
            ..state.manifest.clone()
        },
    )
    .expect("swap state");
    fix.runtime.swap_state(std::sync::Arc::new(new_state));

    let plan = fix.render_plan();
    let hits = fix.runtime.get_feature_info(&plan, (3, 60)).await.expect("gfi");
    assert!(hits.is_empty(), "expected no hits, got {hits:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn gfi_ows_request_gating_overrides_legacy_enable_flag() {
    // regression: capabilities and runtime must agree on the explicit
    // `ows.request_gating.wms_get_feature_info=true` override even when the
    // legacy `wms.enable_get_feature_info` flag is left at its default-false.
    // before this commit gfi.rs read the legacy flag directly and silently
    // dropped layers whose only GFI permission was an explicit OWS gate.
    let fix = build_fixture().await;
    let state = fix.runtime.current_state().expect("state");
    let mut new_cfg: mars_config::Config = state.config.as_ref().unwrap().as_ref().clone();
    if let Some(layer) = new_cfg.layers.iter_mut().find(|l| l.name == fix.layer_id) {
        layer.wms.enable_get_feature_info = false;
        layer
            .ows
            .request_gating
            .insert(mars_config::ServiceOp::WmsGetFeatureInfo, true);
    }
    let new_state = mars_runtime::RuntimeState::from_config_and_manifest(
        &new_cfg,
        state.stylesheet.clone(),
        mars_types::Manifest {
            version: state.manifest.version + 1,
            ..state.manifest.clone()
        },
    )
    .expect("swap state");
    fix.runtime.swap_state(std::sync::Arc::new(new_state));

    let plan = fix.render_plan();
    let hits = fix.runtime.get_feature_info(&plan, (3, 60)).await.expect("gfi");
    assert!(
        !hits.is_empty(),
        "ows gating override should permit GFI even with legacy flag off",
    );
}
