#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use crate::model::SourceBinding;
use crate::validate::fixtures::*;
use crate::validate::validate;

#[test]
fn band_resolves_to_scale_window_for_first_band() {
    let mut cfg = two_band_config();
    let mut b = binding("buildings");
    b.band = Some("hi".into());
    cfg.layers = vec![layer_with_binding(b)];
    validate(&mut cfg, Path::new(".")).expect("validate");
    let scale = cfg.layers[0].sources[0].scale.as_ref().expect("scale set");
    assert_eq!(scale.min, None);
    assert_eq!(scale.max, Some(25_000));
}

#[test]
fn band_resolves_to_scale_window_for_middle_band() {
    let mut cfg = two_band_config();
    let mut b = binding("buildings");
    b.band = Some("mid".into());
    cfg.layers = vec![layer_with_binding(b)];
    validate(&mut cfg, Path::new(".")).expect("validate");
    let scale = cfg.layers[0].sources[0].scale.as_ref().expect("scale set");
    assert_eq!(scale.min, Some(25_000));
    assert_eq!(scale.max, Some(250_000));
}

#[test]
fn band_intersects_with_explicit_scale() {
    let mut cfg = two_band_config();
    let mut b = binding("buildings");
    b.band = Some("mid".into());
    b.scale = Some(crate::model::ScaleWindow {
        min: Some(50_000),
        max: Some(200_000),
    });
    cfg.layers = vec![layer_with_binding(b)];
    validate(&mut cfg, Path::new(".")).expect("validate");
    let scale = cfg.layers[0].sources[0].scale.as_ref().expect("scale set");
    assert_eq!(scale.min, Some(50_000));
    assert_eq!(scale.max, Some(200_000));
}

#[test]
fn band_disjoint_with_explicit_scale_rejected() {
    let mut cfg = two_band_config();
    let mut b = binding("buildings");
    b.band = Some("hi".into());
    // hi covers [0, 25_000); explicit window starts at 50_000.
    b.scale = Some(crate::model::ScaleWindow {
        min: Some(50_000),
        max: Some(100_000),
    });
    cfg.layers = vec![layer_with_binding(b)];
    let err = validate(&mut cfg, Path::new(".")).unwrap_err();
    assert!(
        matches!(err, crate::ConfigError::Invalid(ref s) if s.contains("disjoint")),
        "unexpected error: {err:?}"
    );
}

#[test]
fn band_intersects_with_open_explicit_scale() {
    let mut cfg = two_band_config();
    let mut b = binding("buildings");
    b.band = Some("mid".into());
    // explicit min only; expect mid window's max to win and explicit min
    // to clamp the lower edge above the band's natural min.
    b.scale = Some(crate::model::ScaleWindow {
        min: Some(100_000),
        max: None,
    });
    cfg.layers = vec![layer_with_binding(b)];
    validate(&mut cfg, Path::new(".")).expect("validate");
    let scale = cfg.layers[0].sources[0].scale.as_ref().expect("scale set");
    assert_eq!(scale.min, Some(100_000));
    assert_eq!(scale.max, Some(250_000));
}

#[test]
fn no_band_leaves_explicit_scale_untouched() {
    let mut cfg = two_band_config();
    let mut b = binding("buildings");
    b.band = None;
    b.scale = Some(crate::model::ScaleWindow {
        min: Some(10),
        max: Some(20),
    });
    cfg.layers = vec![layer_with_binding(b)];
    validate(&mut cfg, Path::new(".")).expect("validate");
    let scale = cfg.layers[0].sources[0].scale.as_ref().expect("scale set");
    assert_eq!(scale.min, Some(10));
    assert_eq!(scale.max, Some(20));
}

#[test]
fn two_tiers_in_band_resolve_to_non_overlapping_windows() {
    let mut cfg = two_band_config();
    cfg.layers = vec![tiered_layer(vec![
        SourceBinding {
            band: Some("hi".into()),
            max_denom: Some(8_000),
            from: Some("a".into()),
            ..binding("a")
        },
        SourceBinding {
            band: Some("hi".into()),
            max_denom: Some(25_000),
            from: Some("b".into()),
            ..binding("b")
        },
    ])];
    validate(&mut cfg, Path::new(".")).expect("validate");
    let s0 = cfg.layers[0].sources[0].scale.as_ref().unwrap();
    let s1 = cfg.layers[0].sources[1].scale.as_ref().unwrap();
    assert_eq!(s0.min, None);
    assert_eq!(s0.max, Some(8_000));
    assert_eq!(s1.min, Some(8_000));
    assert_eq!(s1.max, Some(25_000));
}

#[test]
fn back_compat_single_source_no_max_denom_covers_whole_band() {
    let mut cfg = two_band_config();
    cfg.layers = vec![tiered_layer(vec![SourceBinding {
        band: Some("mid".into()),
        max_denom: None,
        from: Some("a".into()),
        ..binding("a")
    }])];
    validate(&mut cfg, Path::new(".")).expect("validate");
    let s = cfg.layers[0].sources[0].scale.as_ref().unwrap();
    assert_eq!(s.min, Some(25_000));
    assert_eq!(s.max, Some(250_000));
}

#[test]
fn duplicate_max_denom_in_band_rejected() {
    let mut cfg = two_band_config();
    cfg.layers = vec![tiered_layer(vec![
        SourceBinding {
            band: Some("hi".into()),
            max_denom: Some(10_000),
            from: Some("a".into()),
            ..binding("a")
        },
        SourceBinding {
            band: Some("hi".into()),
            max_denom: Some(10_000),
            from: Some("b".into()),
            ..binding("b")
        },
    ])];
    let err = validate(&mut cfg, Path::new(".")).unwrap_err();
    assert!(err.to_string().contains("not strictly greater"));
}

#[test]
fn tier_max_denom_exceeds_band_cap_rejected() {
    let mut cfg = two_band_config();
    cfg.layers = vec![tiered_layer(vec![SourceBinding {
        band: Some("hi".into()),
        max_denom: Some(50_000),
        from: Some("a".into()),
        ..binding("a")
    }])];
    let err = validate(&mut cfg, Path::new(".")).unwrap_err();
    assert!(err.to_string().contains("exceeds band cap"));
}

#[test]
fn first_tier_max_at_or_below_band_lower_bound_rejected() {
    // band "mid" spans [25_000, 250_000); a first-tier max of 25_000 (or below) would
    // resolve to an empty window and silently make the source unreachable.
    let mut cfg = two_band_config();
    cfg.layers = vec![tiered_layer(vec![
        SourceBinding {
            band: Some("mid".into()),
            max_denom: Some(25_000),
            from: Some("a".into()),
            ..binding("a")
        },
        SourceBinding {
            band: Some("mid".into()),
            max_denom: Some(250_000),
            from: Some("b".into()),
            ..binding("b")
        },
    ])];
    let err = validate(&mut cfg, Path::new(".")).unwrap_err();
    assert!(
        err.to_string().contains("not strictly greater than band lower bound"),
        "unexpected error: {err}"
    );
}

#[test]
fn non_final_tier_equal_to_band_cap_rejected() {
    let mut cfg = two_band_config();
    cfg.layers = vec![tiered_layer(vec![
        SourceBinding {
            band: Some("hi".into()),
            max_denom: Some(25_000),
            from: Some("a".into()),
            ..binding("a")
        },
        SourceBinding {
            band: Some("hi".into()),
            max_denom: Some(25_000),
            from: Some("b".into()),
            ..binding("b")
        },
    ])];
    let err = validate(&mut cfg, Path::new(".")).unwrap_err();
    assert!(err.to_string().contains("not strictly greater"));
}
