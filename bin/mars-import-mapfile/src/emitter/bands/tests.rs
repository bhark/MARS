#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::emitter::skeleton::{BindingSource, LayerSkeleton, SourceSkeleton};

fn src(max: Option<u64>, from: &str) -> SourceSkeleton {
    SourceSkeleton {
        max_denom_exclusive: max,
        source: BindingSource::Table(from.into()),
        filter: None,
        geometry_column: "g".into(),
        id_column: None,
        attributes: vec![],
    }
}

fn ladder() -> Vec<(String, u64)> {
    vec![
        ("detail".into(), 2_500),
        ("hi".into(), 12_500),
        ("mid".into(), 50_000),
        ("lo".into(), 250_000),
        ("overview".into(), u64::MAX),
    ]
}

#[test]
fn single_open_source_emits_one_tier_per_band() {
    let layer = LayerSkeleton {
        name: "all".into(),
        sources: vec![src(None, "t")],
        ..Default::default()
    };
    let bands = ladder();
    let windows = band_windows(&bands);
    let out = split_layer_into_bands(&layer, &windows);
    assert_eq!(out.len(), 5);
    for (_, tiers) in &out {
        assert_eq!(tiers.len(), 1);
        assert!(tiers[0].max_denom.is_none(), "single-tier band should omit max");
    }
}

#[test]
fn scaletoken_tiers_split_within_a_band() {
    // SCALETOKEN: [0, 1000) -> t0, [1000, MAX) -> t1.
    let layer = LayerSkeleton {
        name: "buildings".into(),
        sources: vec![src(Some(1_000), "t0"), src(None, "t1")],
        ..Default::default()
    };
    let bands = ladder();
    let windows = band_windows(&bands);
    let out = split_layer_into_bands(&layer, &windows);
    let detail = out.iter().find(|(n, _)| *n == "detail").expect("detail band");
    assert_eq!(detail.1.len(), 2);
    assert_eq!(detail.1[0].max_denom, Some(1_000));
    assert_eq!(detail.1[0].src.source_table(), "t0");
    assert!(detail.1[1].max_denom.is_none());
    assert_eq!(detail.1[1].src.source_table(), "t1");
    // every other band has only t1, single-tier, no max.
    for (name, tiers) in &out {
        if *name == "detail" {
            continue;
        }
        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0].src.source_table(), "t1");
        assert!(tiers[0].max_denom.is_none());
    }
}

#[test]
fn partial_band_coverage_is_dropped() {
    // layer caps at 25000 - covers detail and hi fully, mid only partially.
    let layer = LayerSkeleton {
        name: "x".into(),
        sources: vec![src(Some(25_000), "t")],
        ..Default::default()
    };
    let bands = ladder();
    let windows = band_windows(&bands);
    let out = split_layer_into_bands(&layer, &windows);
    let names: Vec<&str> = out.iter().map(|(n, _)| *n).collect();
    assert_eq!(names, vec!["detail", "hi"]);
}
